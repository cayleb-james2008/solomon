//! D8 — Layer 3: the plug-and-play ONBOARDING path (single-tenant first).
//!
//! ============================ WHAT THIS ADDS =============================
//! The product dream, correctly LAST: given ONLY a local project PATH and an API-key ENV name,
//! [`onboard_project`] brings a never-before-managed project into the fleet end-to-end with ZERO
//! hand-editing of repos.json —
//!   1. AUTO-DETECT the stack (gate cmd, language) via the EXISTING read-only stack probe
//!      `control::contracts::detect_stack` (reused, not reinvented);
//!   2. SEED the freshness objective from the project's REAL metric source — a freshness emitter
//!      script that emits the SAME `{metric_id, latest_ts, n_samples, observable}` contract the
//!      whole fleet gates on (`improver::freshness`). Per D1's rule the objective is seeded ONLY when
//!      the emitter script is VERIFIED PRESENT on disk (`registry::resolve_emitter_exists`); with no
//!      real emitter the row is written the HONEST `no_objective: true` sentinel — never a blind
//!      objective (failure catalog #1), and the fleet freshness contract test stays green.
//!   3. PROVISION a per-project ISOLATED state dir `runtime/<name>/` (mirroring today's per-lane
//!      freshness.json / cycle_budget.json isolation) + seed the AGENT.md/backlog.md contracts, and
//!      WRITE the repos.json row (`registry::upsert_onboarded_repo` — the ZERO-hand-editing writer);
//!   4. RUN ONE GATED CYCLE against the isolated state through the EXISTING gates: the freshness
//!      short-circuit gate (`improver::freshness::short_circuit`) then a D4-orchestrator dispatch
//!      (`ceo::orchestrator::dispatch_for_diagnosis`) — no new authority, every existing gate decides.
//!
//! ========================================================================
//!
//! ## Multi-tenant isolation is DESIGNED, single-tenant is the done-bar (ponytail scope discipline)
//!
//! The per-project runtime dir + the isolated `Ctx` are the isolation seam a future multi-tenant
//! runtime needs (each tenant is a `runtime/<name>/` subtree + its own `Ctx`; nothing here is
//! fleet-global). But this module deliberately does NOT build a multi-tenant scheduler, a tenant
//! registry, or per-tenant credentials — none is forced until ONE lane is profitable. The done-bar
//! is ONE new project onboarded end-to-end SINGLE-TENANT; multi-tenant is scaffolded (the isolation
//! is real) and validated no further.
//!
//! ## What this module does NOT do (the moat: honesty, no new authority)
//!
//!   * It creates NO new gate and WEAKENS none. The gated cycle it runs is the EXISTING freshness +
//!     orchestrator path; the money_guard / pecrt::safety / blast-radius / skeptic / kill gates are
//!     untouched and still decide.
//!   * It never commits a secret. The onboarding contract is (path + api-key-ENV NAME); the env NAME
//!     is stored in the row (like every lane), the secret VALUE is resolved at run time from the
//!     environment, never written to repos.json.
//!   * It claims progress ONLY from the real gated-cycle outcome. A gate SKIP (unobservable / no new
//!     data) is reported honestly as `gated_cycle: "skipped"`, never as a fabricated success.
#![allow(dead_code)]

use crate::control::{contracts, paths, registry};
use crate::improver::ctx::Ctx;
use serde_json::{Value, json};
use std::path::Path;

/// Candidate freshness-emitter commands, in priority order. Each is the SAME shape the fleet's real
/// lanes use (an interpreter + a repo-relative script that emits the freshness JSON contract): the
/// kairos template is `.venv\Scripts\python tools\fitness.py --freshness`; asmodeus/sover use
/// `tools\freshness.py`. Onboarding tries each: the FIRST whose emitter script EXISTS on disk (per
/// D1's rule, `registry::resolve_emitter_exists`) becomes the seeded objective. If none exists, the
/// lane is written the honest `no_objective` sentinel — never a blind objective.
///
/// `{py}` is substituted with the resolved interpreter prefix (the project's `.venv` python when
/// present, else `python`) so the emitted cmd runs on the onboarded box.
const EMITTER_CANDIDATES: &[&str] = &[
    "{py} tools/fitness.py --freshness",
    "{py} tools/freshness.py",
    "{py} tools/freshness.py --json",
];

/// The interpreter prefix for the onboarded project: its own `.venv` python when present (matching
/// `contracts::detect_stack`'s venv detection), else bare `python`. Native separators so the emitted
/// cmd runs through the shell on the onboarding box.
fn interpreter_prefix(project_path: &str) -> String {
    let base = Path::new(project_path);
    let win = cfg!(windows);
    let venv_rel = if win {
        base.join(".venv").join("Scripts").join("python.exe")
    } else {
        base.join(".venv").join("bin").join("python")
    };
    if venv_rel.exists() {
        if win {
            r".venv\Scripts\python".to_string()
        } else {
            ".venv/bin/python".to_string()
        }
    } else {
        "python".to_string()
    }
}

/// Seed the freshness disposition for a project by probing for a REAL emitter script on disk. Pure
/// over the project path (the unit-tested core): returns the freshness block to write into the
/// repos.json row —
///   * `{"cmd": "<emitter>", "min_new_samples": 1, "hard": true, "max_starve_cycles": 16}` when a
///     candidate emitter SCRIPT EXISTS on disk (the objective is real + observable-gated, exactly
///     like the kairos template), OR
///   * `{"no_objective": true}` when NO candidate emitter exists — the honest sentinel (D1's rule:
///     seed a real objective or declare none; never a blind objective).
///
/// Returns `(freshness_block, seeded_cmd_or_none)` so the caller can report WHICH real metric source
/// (if any) was seeded.
pub fn seed_freshness(project_path: &str) -> (Value, Option<String>) {
    let py = interpreter_prefix(project_path);
    for template in EMITTER_CANDIDATES {
        let cmd = template.replace("{py}", &py);
        // Build the transient row shape resolve_emitter_exists reads: {path, freshness:{cmd}}.
        let probe_row = json!({
            "name": "__onboard_probe__",
            "path": project_path,
            "freshness": {"cmd": cmd},
        });
        if let Some((_resolved, true)) = registry::resolve_emitter_exists(&probe_row) {
            // Emitter script VERIFIED present on disk — seed the real objective (kairos template).
            let block = json!({
                "cmd": cmd,
                "min_new_samples": 1,
                "hard": true,
                "max_starve_cycles": 16,
            });
            return (block, Some(cmd));
        }
    }
    // No real emitter — the honest no-objective sentinel (never a blind objective).
    (json!({"no_objective": true}), None)
}

/// The onboarding result, on success. Every field is EVIDENCE of a real step, never a claim: the
/// detected stack, the seeded objective (or the honest no_objective note), the ISOLATED runtime dir
/// created, and the ONE gated-cycle disposition.
#[derive(Debug)]
pub struct Onboarded {
    pub name: String,
    pub path: String,
    pub gate: String,
    pub lang: String,
    pub freshness_cmd: Option<String>,
    pub runtime_dir: std::path::PathBuf,
    pub created_row: bool,
    /// The ONE gated cycle's honest disposition: "ran" (the gate proceeded and the orchestrator
    /// dispatched), or "skipped: <reason>" (the freshness gate short-circuited — unobservable / no
    /// new data / hold). NEVER a fabricated success.
    pub gated_cycle: String,
}

/// Onboard a never-before-managed LOCAL project from (path + api-key-ENV) ALONE. Returns a
/// `{ok:true, ...}` evidence dict on success or `{ok:false, error}` on a fail-closed refusal (path
/// not a dir, no detectable gate, corrupt repos.json). ZERO hand-editing of repos.json: this writes
/// the row. `provider` defaults to the fleet default (ollama-cloud) when None; `goal` is the
/// operator's optional north-star (a lane with no goal is dormant by the CEO plane's rule, so a
/// caller that wants the lane planned should pass one).
///
/// `run_cycle=false` skips step 4 (used by tests that only need to prove steps 1-3 without spawning
/// pi); production onboarding passes `true`.
pub fn onboard_project(
    path: &str,
    api_key_env: &str,
    goal: Option<&str>,
    provider: Option<&str>,
    run_cycle: bool,
) -> Value {
    // (0) Resolve + validate the target. A non-directory is a fail-closed refusal (we never onboard
    // a path we cannot read a stack from).
    let project_path = abspath(path.trim());
    if project_path.is_empty() || !Path::new(&project_path).is_dir() {
        return json!({"ok": false, "error": format!("project path is not a directory: {path}")});
    }
    let name = Path::new(&project_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return json!({"ok": false, "error": "cannot derive a project name from the path basename"});
    }
    if api_key_env.trim().is_empty() {
        return json!({"ok": false, "error": "an API-key ENV name is required (e.g. OLLAMA_API_KEY)"});
    }

    // (1) AUTO-DETECT the stack via the EXISTING read-only probe. A project with no detectable test
    // command has no gate — we refuse rather than onboard a lane that cannot be gated honestly.
    let stack = contracts::detect_stack(&project_path);
    let lang = stack
        .get("lang")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let gate = stack
        .get("test_cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if gate.trim().is_empty() {
        return json!({
            "ok": false,
            "error": format!(
                "no gate command auto-detected for {name} (lang={lang}) — a lane must have a gate to \
                 verify a change before it ships; add a test runner or onboard with an explicit gate"
            ),
        });
    }

    // (2) SEED the freshness objective from the project's REAL metric source (emitter verified
    // present per D1's rule), else the honest no_objective sentinel.
    let (freshness_block, freshness_cmd) = seed_freshness(&project_path);

    // (3) WRITE the repos.json row (ZERO hand-editing) — the onboarding writer sets path + gate +
    // freshness + api-key ENV + goal + provider in one atomic upsert.
    let write = registry::upsert_onboarded_repo(
        &name,
        &project_path,
        &gate,
        goal,
        provider,
        Some(api_key_env),
        Some(&freshness_block),
    );
    if !write.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return json!({
            "ok": false,
            "error": format!("failed to write repos.json row: {}", write.get("error").cloned().unwrap_or(Value::Null)),
        });
    }
    let created_row = write
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Build the repo Value the contract/isolation steps read (from the freshly-written row, so we use
    // exactly what onboarding persisted — not a hand-built shape).
    let repo = row_for(&name).unwrap_or_else(|| json!({"name": name, "path": project_path}));

    // (3b) PROVISION the ISOLATED runtime dir + seed AGENT.md/backlog.md. runtime/<name>/ is created
    // UNDER Solomon (never inside the product) — the per-project isolation seam.
    let runtime_dir = match paths::runtime_dir(&repo) {
        Some(d) => d,
        None => {
            return json!({"ok": false, "error": "could not resolve an isolated runtime dir for the lane"});
        }
    };
    if let Err(e) = std::fs::create_dir_all(&runtime_dir) {
        return json!({"ok": false, "error": format!("could not create isolated runtime dir: {e}")});
    }
    let contracts_val = contracts::ensure_contracts(&repo);

    // (4) RUN ONE GATED CYCLE against the isolated state, through the EXISTING gates. The freshness
    // short-circuit runs FIRST (it reads the lane's own runtime/<name>/freshness.json — isolated);
    // if it does not skip, dispatch a code task through the D4 orchestrator (money_guard +
    // pecrt::safety decide). We report the HONEST disposition either way — a skip is not a failure.
    let gated_cycle = if run_cycle {
        run_one_gated_cycle(&repo)
    } else {
        "not_run (run_cycle=false)".to_string()
    };

    json!({
        "ok": true,
        "name": name,
        "path": project_path,
        "gate": gate,
        "lang": lang,
        "freshness_seeded": freshness_cmd.is_some(),
        "freshness_cmd": freshness_cmd,
        "no_objective": freshness_cmd.is_none(),
        "runtime_dir": runtime_dir.to_string_lossy(),
        "created_row": created_row,
        "contracts": contracts_val,
        "gated_cycle": gated_cycle,
    })
}

/// Run ONE gated cycle for an onboarded lane against its ISOLATED state, returning the honest
/// disposition string. Builds the lane's own `Ctx` (runtime under `runtime/<name>/`), then:
///   * the freshness short-circuit gate runs first (reads the lane's own freshness ledger) — a SKIP
///     is reported honestly, never as a success;
///   * on proceed, a code task is dispatched through the D4 orchestrator, whose fail-closed gate
///     (money_guard + pecrt::safety) decides. The dispatch outcome's ok-ness is reported verbatim.
fn run_one_gated_cycle(repo: &Value) -> String {
    let name = paths::repo_name(repo);
    let path = paths::repo_path(repo);
    let provider = registry::project_provider(repo);
    let model = registry::project_model(repo);
    let mut ctx = Ctx::configure(
        &path,
        &name,
        &provider,
        if model.is_empty() {
            None
        } else {
            Some(model.as_str())
        },
    );

    // The EXISTING freshness gate — reads THIS lane's runtime/<name>/freshness.json (isolated). It
    // returns true to SKIP (unobservable / no new data / hold). An honest skip is a real outcome.
    if crate::improver::freshness::short_circuit(&mut ctx) {
        return "skipped: freshness gate short-circuited (unobservable / no new objective data / hold) \
                — honest no-op, not a failure"
            .to_string();
    }

    // Proceed: dispatch ONE code task through the D4 orchestrator. The orchestrator's fail-closed
    // gate decides; a plain code task on an ordinary lane is admitted and the downstream gates still
    // rule. We do not force a ship — the runner owns version control.
    let outcome = crate::ceo::orchestrator::dispatch_for_diagnosis(
        repo,
        "needs_goal",
        "onboarding smoke cycle: implement the top backlog item as one small, tested change",
    );
    let ran_ok = outcome.get("ok").and_then(Value::as_bool).unwrap_or(false);
    format!(
        "ran: freshness gate proceeded; orchestrator dispatched (ok={ran_ok}, reason={})",
        outcome.get("reason").and_then(Value::as_str).unwrap_or(
            outcome
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("dispatched")
        )
    )
}

/// The freshly-persisted repos.json row for `name` (merged view, so discovered + config layer). None
/// when the lane is absent (should not happen right after a successful upsert).
fn row_for(name: &str) -> Option<Value> {
    registry::load_repos()
        .into_iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(name))
}

/// TEST-ONLY: drop the just-onboarded repos.json row so parallel tests don't leak transport rows
/// into the operator's repos.json across `cargo test` runs (the rows otherwise accumulate forever —
/// `cargo test` discovery has been adding tens of `solomon_onboard_e2e_*` rows after every CI cycle).
/// Best-effort: rsync of read -> mutate -> atomic write is racy with concurrent onboard writers,
/// but the file-level TOCTOU is at worst a single dropped-cleanup on the next test run (the row is
/// name-tagged, so a subsequent test that doesn't recognize the leaked row falls back to "not
/// present" and re-runs onboarding cleanly). Never reachable from production code paths.
///
/// Caller MUST already hold `registry::lock_repos_for_test()` — non-reentrant std Mutex would
/// deadlock if this fn re-acquired it from the same thread.
#[cfg(test)]
fn drop_onboarded_row(name: &str) {
    let Ok(mut entries) = registry::read_repos_for_write() else {
        return;
    };
    entries.retain(|r| r.get("name").and_then(Value::as_str) != Some(name));
    let _ = registry::write_repo_entries(&entries);
}

/// TEST-ONLY: acquire the process-global repos.json writer lock for the duration of the test body —
/// guards the read+modify+write of `onboard_project` against any parallel repos.json-writing test
/// (`control::registry::set_repo_config_api_key_round_trip` holds the same lock). Without this
/// guard, parallel `cargo test` runs occasional corrupt the live operator's `repos.json` when
/// multiple threads observe and write to the file concurrently. Best-effort: a poisoned lock is
/// recovered into the held guard instead of panicking — the write after panic can still progress.
#[cfg(test)]
pub(crate) fn drop_repos_lock_for_test() -> registry::ReposLockGuard {
    registry::lock_repos_for_test()
}

/// Absolute, normalized path (lexical — no existence requirement), matching the registry's own
/// abspath so an onboarded row's `path` is byte-identical to a discovered one.
fn abspath(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    abs.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A throwaway project dir with a python manifest + a tests dir, so detect_stack yields a real
    /// gate. Unique per test so parallel runs never collide. Optionally seeds a `tools/fitness.py`
    /// emitter so seed_freshness finds a real objective.
    fn tmp_project(tag: &str, with_emitter: bool) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "solomon_onboard_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(d.join("tests")).unwrap();
        std::fs::write(d.join("requirements.txt"), "").unwrap();
        std::fs::write(
            d.join("tests").join("test_x.py"),
            "def test_ok():\n    assert True\n",
        )
        .unwrap();
        if with_emitter {
            std::fs::create_dir_all(d.join("tools")).unwrap();
            std::fs::write(d.join("tools").join("fitness.py"), "print('{}')\n").unwrap();
        }
        d
    }

    // ===================================================================== #
    // D8 ACCEPTANCE (a): a never-before-managed LOCAL project is onboarded from
    // (path + api-key-ENV) ALONE — stack auto-detected, freshness objective
    // seeded (emitter verified present per D1's rule), ISOLATED runtime/<name>/
    // created, ONE gated cycle runs — with ZERO hand-editing of repos.json.
    // ===================================================================== #
    #[test]
    fn onboards_a_local_project_end_to_end_single_tenant() {
        // Hold BOTH the global repos.json writer lock (parallel test race on the live operator
        // repos.json) and the notify env lock (avoid parallel notify side-effects) for the whole
        // test body. Drop happens in reverse-source order at scope exit.
        let _repos_guard = drop_repos_lock_for_test();
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        let proj = tmp_project("e2e", true); // WITH a real tools/fitness.py emitter
        let proj_s = proj.to_string_lossy().into_owned();

        // Onboard from PATH + API-KEY-ENV alone. run_cycle=true exercises the real gated cycle.
        let out = onboard_project(
            &proj_s,
            "OLLAMA_API_KEY",
            Some("ship one small improvement"),
            None,
            true,
        );
        assert_eq!(out["ok"], true, "onboarding must succeed: {out}");

        let name = out["name"].as_str().unwrap();

        // (1) stack auto-detected: a python gate.
        assert_eq!(out["lang"], "python");
        assert!(
            out["gate"].as_str().unwrap().contains("pytest")
                || out["gate"].as_str().unwrap().contains("unittest"),
            "a real gate was detected: {}",
            out["gate"]
        );

        // (2) freshness objective SEEDED from the real emitter (verified present per D1's rule).
        assert_eq!(
            out["freshness_seeded"], true,
            "the real emitter must seed a freshness objective: {out}"
        );
        assert!(
            out["freshness_cmd"]
                .as_str()
                .unwrap()
                .contains("tools/fitness.py")
        );
        assert_eq!(out["no_objective"], false);

        // (3) ZERO hand-editing: the row was WRITTEN by onboarding and is now in repos.json with the
        // seeded objective + gate + api-key ENV + path.
        let row = row_for(name).expect("the onboarded row is in repos.json");
        assert_eq!(row["path"].as_str().unwrap(), out["path"].as_str().unwrap());
        assert_eq!(
            row["api_key"],
            json!("OLLAMA_API_KEY"),
            "the api-key ENV name is stored (never a secret)"
        );
        assert!(
            row["freshness"]["cmd"]
                .as_str()
                .unwrap()
                .contains("tools/fitness.py")
        );
        assert!(
            row["gate"].as_str().unwrap().contains("pytest")
                || row["gate"].as_str().unwrap().contains("unittest")
        );

        // (3b) ISOLATED runtime/<name>/ created UNDER Solomon (never inside the product repo).
        let rt = std::path::Path::new(out["runtime_dir"].as_str().unwrap());
        assert!(
            rt.is_dir(),
            "the isolated runtime dir must exist: {}",
            rt.display()
        );
        assert!(
            rt.ends_with(name),
            "the runtime dir is per-project isolated: {}",
            rt.display()
        );
        assert!(
            !rt.starts_with(&proj),
            "isolation: runtime state is NOT inside the product repo"
        );

        // (4) ONE gated cycle ran — the freshness gate ran against the ISOLATED ledger. With a real
        // emitter that prints `{}` (no metric_id / observable) the objective reads UNOBSERVABLE, so
        // the gate HONESTLY skips — reported as a skip, never a fabricated success.
        let disp = out["gated_cycle"].as_str().unwrap();
        assert!(
            disp.starts_with("skipped") || disp.starts_with("ran"),
            "the gated cycle reports an honest disposition (ran/skipped): {disp}"
        );

        // cleanup: the isolated runtime subtree + the temp project + the leaked repos.json row.
        // The repos.json row removal prevents (a) stacking test rows into the operator's config
        // across every `cargo test` run and (b) read-modify-write races with other parallel
        // onboard tests that both target the same live repos.json. ponytail: drop the side effect
        // at the source rather than serializing the whole e2e block.
        let _ = std::fs::remove_dir_all(rt);
        let _ = std::fs::remove_dir_all(&proj);
        drop_onboarded_row(name);
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    // ===================================================================== #
    // D8 ACCEPTANCE (a, honesty floor / D1's rule): a project with NO real
    // emitter is onboarded with the HONEST no_objective sentinel — never a
    // blind objective. So the fleet freshness contract stays satisfiable.
    // ===================================================================== #
    #[test]
    fn a_project_without_an_emitter_is_onboarded_no_objective_not_blind() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        let proj = tmp_project("noemit", false); // NO tools/fitness.py
        let proj_s = proj.to_string_lossy().into_owned();

        let out = onboard_project(&proj_s, "OLLAMA_API_KEY", None, None, false);
        assert_eq!(out["ok"], true, "{out}");
        // No emitter on disk -> the honest sentinel, not a fabricated blind objective.
        assert_eq!(out["freshness_seeded"], false);
        assert_eq!(out["no_objective"], true);
        let name = out["name"].as_str().unwrap();
        let row = row_for(name).expect("row present");
        assert_eq!(
            row["freshness"]["no_objective"],
            json!(true),
            "the honest sentinel is persisted: {row}"
        );
        // and it satisfies the fleet freshness disposition classifier as an explicit opt-out.
        assert_eq!(
            registry::freshness_disposition(&row),
            registry::FreshnessDisposition::NoObjective
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(out["runtime_dir"].as_str().unwrap()));
        let _ = std::fs::remove_dir_all(&proj);
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    // ===================================================================== #
    // D8 ACCEPTANCE (c): the onboarded project's gate + freshness run WITHOUT
    // touching any other lane's state (per-project isolation verified).
    // ===================================================================== #
    #[test]
    fn onboarded_lane_state_is_isolated_from_other_lanes() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        // Onboard TWO distinct projects; each must get its OWN runtime/<name>/ subtree, and running
        // one lane's gated cycle must write only under THAT lane's dir — never the other's.
        let a = tmp_project("isoA", true);
        let b = tmp_project("isoB", true);
        let out_a = onboard_project(&a.to_string_lossy(), "OLLAMA_API_KEY", None, None, false);
        let out_b = onboard_project(&b.to_string_lossy(), "OLLAMA_API_KEY", None, None, false);
        assert_eq!(out_a["ok"], true);
        assert_eq!(out_b["ok"], true);

        let rt_a = std::path::PathBuf::from(out_a["runtime_dir"].as_str().unwrap());
        let rt_b = std::path::PathBuf::from(out_b["runtime_dir"].as_str().unwrap());
        assert_ne!(
            rt_a, rt_b,
            "each onboarded lane has a DISTINCT isolated runtime dir"
        );

        // Snapshot lane B's runtime dir contents, then run lane A's gated cycle. B must be untouched.
        let b_before = list_dir(&rt_b);
        let repo_a = row_for(out_a["name"].as_str().unwrap()).unwrap();
        let _disp = run_one_gated_cycle(&repo_a);
        let b_after = list_dir(&rt_b);
        assert_eq!(
            b_before, b_after,
            "running lane A's gated cycle must not touch lane B's isolated state"
        );
        // and A's freshness ledger (if the gate wrote one) lives under A's dir, not B's.
        assert!(
            !rt_b.join("freshness.json").exists(),
            "lane A's freshness gate never wrote into lane B's dir"
        );

        let _ = std::fs::remove_dir_all(&rt_a);
        let _ = std::fs::remove_dir_all(&rt_b);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    // ---- fail-closed refusals ----
    #[test]
    fn refuses_a_non_directory_path() {
        let out = onboard_project(
            "C:/no/such/onboard/path/zzz",
            "OLLAMA_API_KEY",
            None,
            None,
            false,
        );
        assert_eq!(out["ok"], false);
        assert!(out["error"].as_str().unwrap().contains("not a directory"));
    }

    #[test]
    fn refuses_a_project_with_no_detectable_gate() {
        // an empty dir: detect_stack yields lang=unknown, test_cmd="" -> refuse (no honest gate).
        let d = std::env::temp_dir().join(format!(
            "solomon_onboard_nogate_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        let out = onboard_project(&d.to_string_lossy(), "OLLAMA_API_KEY", None, None, false);
        assert_eq!(out["ok"], false, "{out}");
        assert!(out["error"].as_str().unwrap().contains("no gate command"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_an_empty_api_key_env() {
        let proj = tmp_project("nokey", false);
        let out = onboard_project(&proj.to_string_lossy(), "   ", None, None, false);
        assert_eq!(out["ok"], false);
        assert!(out["error"].as_str().unwrap().contains("API-key ENV"));
        let _ = std::fs::remove_dir_all(&proj);
    }

    // ---- seed_freshness pure core ----
    #[test]
    fn seed_freshness_seeds_real_objective_when_emitter_present_else_no_objective() {
        let with = tmp_project("seedwith", true);
        let (block, cmd) = seed_freshness(&with.to_string_lossy());
        assert!(cmd.is_some(), "an emitter on disk seeds a real cmd");
        assert!(block["cmd"].as_str().unwrap().contains("tools/fitness.py"));
        assert_eq!(block["hard"], json!(true));
        assert_eq!(block["min_new_samples"], json!(1));
        let _ = std::fs::remove_dir_all(&with);

        let without = tmp_project("seedwithout", false);
        let (block2, cmd2) = seed_freshness(&without.to_string_lossy());
        assert!(cmd2.is_none(), "no emitter -> the honest sentinel");
        assert_eq!(block2, json!({"no_objective": true}));
        let _ = std::fs::remove_dir_all(&without);
    }

    /// A sorted list of a dir's entry names (for the isolation before/after comparison).
    fn list_dir(d: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}
