//! Closed action registry + TTL escalation policies (RSI v3; failure catalog #3: "detection
//! without actuation — every diagnosis dead-ends at 'wait for operator'").
//!
//! Solomon Gen-2's planner recommended a job kind that did not exist; sover's watchdog invoked a
//! nonexistent CLI action every 15 minutes for weeks. This module closes that gap STRUCTURALLY:
//!
//!   * `actions.json` (versioned, at the Solomon root beside repos.json) maps EVERY category
//!     `supervisor::diagnose` can emit to a primary remediation + a degraded-mode fallback, each
//!     drawn from the CLOSED kind set [`ACTION_KINDS`];
//!   * the closure #[test] below asserts the mapping is TOTAL and uses only closed kinds — cargo
//!     test (the build gate) goes red the moment a diagnosis exists without an executable
//!     remediation;
//!   * every kind is auto-safe and reuses machinery that already exists (runner start, the RUNG-0
//!     reset, the PR-gated fix-session, budget-ledger parking, marker-deduped paging). Nothing
//!     here force-kills, force-pushes, or bypasses a gate.
//!
//! `supervisor::recover` consumes [`policy_for`] + [`execute_action`] in its TTL pre-step: an
//! escalation that outlives its TTL executes its registered fallback instead of waiting forever.

use crate::control::{branches, locks, paths, registry, runner};
use crate::notify;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The CLOSED set of executable action kinds. Every `action`/`fallback` in actions.json must be a
/// member (closure test — CI-red otherwise), and [`execute_action`]'s `_` arm refuses anything
/// else with `{ok:false}` instead of guessing. Adding a kind means adding BOTH its match arm and
/// (via the test) its registry acceptance in one commit.
pub const ACTION_KINDS: &[&str] = &[
    "none",
    "restart_lane",
    "reset_to_base",
    "run_fix_session",
    "park_primary_endpoint",
    "clear_escalation_and_retry",
    "page_operator_deduped",
];

/// The degraded-mode terminal action: when a category's fallbacks are exhausted (or its mapping is
/// missing/corrupt) the engine pages the operator — marker-deduped, never a page storm, never a
/// silent dead end.
pub const DEGRADED_KIND: &str = "page_operator_deduped";

/// Fallback TTL when actions.json is missing/corrupt or carries no usable default_ttl_s. A
/// hardcoded floor so a deleted data file can never produce a TTL of 0 (= fallback storm) or
/// no TTL at all (= the dead end this module exists to kill).
pub const DEFAULT_TTL_S: u64 = 3_600;

/// One page per category per this window (the 310-undeduped-housekeeping-pages lesson).
pub const PAGE_DEDUP_WINDOW_S: u64 = 86_400;

/// One category's TTL policy row.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// The documented PRIMARY remediation for the category (the existing recover() ladder is the
    /// machinery that performs it; recorded here so the registry is a complete diagnosis→action
    /// map for planners and the closure test). Not executed by the TTL pre-step.
    #[allow(dead_code)] // consumed by the closure test + future planners, not the runtime path
    pub action: String,
    /// Seconds an escalation may stand before the fallback executes.
    pub ttl_s: u64,
    /// The degraded-mode action executed on TTL expiry.
    pub fallback: String,
    /// Fallback executions before degrading to the daily deduped page.
    pub max_fallback_runs: u64,
}

/// `<solomon root>/actions.json` — versioned DATA, beside repos.json, editable without a rebuild
/// (the closure test still pins its SHAPE at build time).
pub fn actions_json_path() -> PathBuf {
    paths::here().join("actions.json")
}

/// Parse a document; a missing/corrupt/non-object file degrades to `{}` so [`policy_from`] serves
/// the safe default policy. Never a panic: policy lookups run on the watchdog sweep thread.
/// A UTF-8 BOM is stripped first — Windows editors (and PowerShell 5.1 Out-File) prepend one, and
/// a hand-edited data file must degrade to defaults for CONTENT reasons only, not encoding trivia.
fn load_doc_from(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(t.trim_start_matches('\u{feff}')).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// The live actions.json document (re-read per lookup: it is tiny and the sweep is 2-minutely, so
/// an operator edit takes effect without a restart).
pub fn load_doc() -> Value {
    load_doc_from(&actions_json_path())
}

fn doc_default_ttl(doc: &Value) -> u64 {
    doc.get("default_ttl_s")
        .and_then(Value::as_u64)
        .filter(|t| *t > 0)
        .unwrap_or(DEFAULT_TTL_S)
}

/// Pure policy lookup (unit-testable with an injected doc). An UNKNOWN category — a diagnosis the
/// registry has never heard of (the closure test makes this impossible for diagnose()'s own set,
/// but escalation.json is a disk file anything may have written) — gets the default TTL and the
/// deduped operator page: never a panic, never a silent dead end.
pub fn policy_from(doc: &Value, category: &str) -> Policy {
    let default_ttl = doc_default_ttl(doc);
    match doc
        .get("diagnoses")
        .and_then(|d| d.get(category))
        .and_then(Value::as_object)
    {
        None => Policy {
            action: DEGRADED_KIND.to_string(),
            ttl_s: default_ttl,
            fallback: DEGRADED_KIND.to_string(),
            max_fallback_runs: 3,
        },
        Some(o) => Policy {
            action: o
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or(DEGRADED_KIND)
                .to_string(),
            ttl_s: o
                .get("ttl_s")
                .and_then(Value::as_u64)
                .filter(|t| *t > 0)
                .unwrap_or(default_ttl),
            fallback: o
                .get("fallback")
                .and_then(Value::as_str)
                .unwrap_or(DEGRADED_KIND)
                .to_string(),
            max_fallback_runs: o.get("max_fallback_runs").and_then(Value::as_u64).unwrap_or(3),
        },
    }
}

/// The TTL policy for one diagnosis category, from the live actions.json.
pub fn policy_for(category: &str) -> Policy {
    policy_from(&load_doc(), category)
}

/// Execute one CLOSED-SET action kind against a repo row. Total: every kind returns a Value with
/// an honest `ok`; an unknown kind returns `{ok:false, error}` (the sover lesson — an engine must
/// never keep "invoking" an action that does not exist, and must never panic on a data-file typo).
///
/// `category` threads the diagnosis identity into the page-dedup marker; `auto_push` threads the
/// same global ship gate `recover()` already threads to its own restart/fix-session spawns.
pub fn execute_action(kind: &str, repo: &Value, category: &str, auto_push: bool) -> Value {
    // OUTERMOST default-DENY: the NO-MONEY-OUT guard. Checked BEFORE any action
    // machinery runs, so no money-capable action can execute without passing it.
    // Returns Some(refusal) for a DENIED money action (money-out / unknown /
    // ambiguous — fail-closed), None to proceed. Transparent to every non-money
    // kind. See money_guard.rs for the HARD invariant + doctrine lineage.
    if let Some(refusal) = crate::money_guard::guard(kind, repo) {
        return refusal;
    }
    match kind {
        // healthy / nothing to do — an explicit no-op so "every diagnosis maps" includes "ok".
        "none" => json!({"ok": true, "kind": "none", "detail": "no action required"}),

        // The same control start path the watchdog's should_restart uses (runner::start clears a
        // lingering stop sentinel itself and no-ops with {already:true} on a live lane).
        "restart_lane" => {
            let r = runner::start(repo, auto_push, false);
            let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
            json!({"ok": ok, "kind": "restart_lane", "result": r})
        }

        "reset_to_base" => reset_to_base_action(repo),

        // The existing PR-gated solomon_fix_session: a TTL-forced fix is still a reviewable PR.
        "run_fix_session" => {
            if locks::is_running(repo) {
                return json!({
                    "ok": false, "kind": "run_fix_session",
                    "error": "loop is live — not spawning a fix-session alongside it",
                });
            }
            let r = crate::supervisor::solomon_fix_session(repo, auto_push);
            let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
            json!({"ok": ok, "kind": "run_fix_session", "result": r})
        }

        "park_primary_endpoint" => park_primary_endpoint(repo),

        // Retire the standing escalation and give the lane a fresh start. The clear IS the
        // actuation (recover()'s TTL pre-step deliberately does not resurrect the file after it).
        "clear_escalation_and_retry" => {
            let cleared = crate::supervisor::clear_escalation(repo);
            let r = runner::start(repo, auto_push, false);
            let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
            json!({"ok": ok, "kind": "clear_escalation_and_retry", "cleared": cleared, "result": r})
        }

        "page_operator_deduped" => page_operator_deduped(repo, category),

        other => json!({
            "ok": false, "kind": other,
            "error": format!(
                "unknown action kind '{other}' — not in the closed registry {ACTION_KINDS:?}"
            ),
        }),
    }
}

/// The RUNG-0 reset recover() already performs, with the same two guards: never reset a live-app
/// repo (its running app dirties its own tree by design) and never reset under a live loop
/// (supervisor lock required).
fn reset_to_base_action(repo: &Value) -> Value {
    if matches!(repo.get("live_app"), Some(Value::Bool(true))) {
        return json!({
            "ok": false, "kind": "reset_to_base",
            "error": "live-app repo — not resetting a tree its own app writes into",
        });
    }
    let (ok, token) = locks::acquire_supervisor_lock(repo);
    if !ok {
        return json!({
            "ok": false, "kind": "reset_to_base",
            "error": "loop is live — stop it before resetting the base",
        });
    }
    let r = branches::reset_to_base(repo);
    if let Some(tok) = token {
        locks::release_supervisor_lock(repo, &tok);
    }
    let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
    json!({"ok": ok, "kind": "reset_to_base", "result": r})
}

/// The provider-swap remediation (catalog #2 meets #3): park the repo's PRIMARY endpoint in the
/// fleet budget ledger via `budget::record_quota`, so `budget::effective_endpoint` resolves the
/// repo's canaried fallback endpoint on the next pi call. The Ctx is built exactly the way
/// run-improver builds it so the parked ledger key ("<pi_provider>:<pi_model>") is byte-identical
/// to the key run_pi's preflight reads — parking any other key would be remediation theater.
fn park_primary_endpoint(repo: &Value) -> Value {
    let name = paths::repo_name(repo);
    if name.is_empty() {
        return json!({"ok": false, "kind": "park_primary_endpoint", "error": "repo has no name"});
    }
    let path = paths::repo_path(repo);
    let provider = registry::project_provider(repo);
    let model = registry::project_model(repo);
    let ctx = crate::improver::ctx::Ctx::configure(
        &path,
        &name,
        &provider,
        if model.is_empty() { None } else { Some(model.as_str()) },
    );
    let until = crate::improver::budget::record_quota(&ctx, &ctx.pi_provider, &ctx.pi_model);
    json!({
        "ok": true, "kind": "park_primary_endpoint",
        "endpoint": format!("{}:{}", ctx.pi_provider, ctx.pi_model),
        "park_until": until,
        "detail": "primary endpoint parked in the provider budget ledger — effective_endpoint \
                   resolves the canaried fallback on the next call",
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// runtime/<name>/_paged_<category>. Diagnosis categories are [a-z_] identifiers, but the
/// category can arrive from a disk escalation.json anything may have written — sanitize so it can
/// never steer the marker outside the runtime dir.
fn page_marker_path(dir: &Path, category: &str) -> PathBuf {
    let safe: String = category
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' })
        .collect();
    dir.join(format!("_paged_{safe}"))
}

/// True when this category already paged within the dedup window. The marker stores the unix
/// send-second; a missing/unparseable marker reads as "due" (page, then rewrite a good marker).
fn page_deduped_at(dir: &Path, category: &str, now: u64) -> bool {
    std::fs::read_to_string(page_marker_path(dir, category))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .map(|sent| now.saturating_sub(sent) < PAGE_DEDUP_WINDOW_S)
        .unwrap_or(false)
}

/// Page the operator about a category, at most once per 24h per category (marker-deduped). The
/// marker is stamped BEFORE the send: a crash between the two suppresses at most one page, while
/// the reverse order can storm (catalog #8 — 310 undeduped pages buried the one real alarm).
fn page_operator_deduped(repo: &Value, category: &str) -> Value {
    let dir = match paths::runtime_dir(repo) {
        Some(d) => d,
        None => {
            return json!({"ok": false, "kind": "page_operator_deduped", "error": "repo has no runtime dir"})
        }
    };
    let now = unix_now();
    if page_deduped_at(&dir, category, now) {
        return json!({"ok": true, "kind": "page_operator_deduped", "deduped": true});
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(page_marker_path(&dir, category), format!("{now}"));
    let name = paths::repo_name(repo);
    let notice = notify::Notice::red(
        format!("Solomon: {name} {category} unresolved"),
        format!(
            "escalation TTL expired — operator action required; \
             see runtime/{name}/escalation.json for the diagnosis and suggested steps"
        ),
    );
    let sent = notify::send(&notice);
    json!({"ok": true, "kind": "page_operator_deduped", "deduped": false, "sent": sent})
}

// --------------------------------------------------------------------------- #
// tests — including the BUILD-TIME CLOSURE CONTRACT
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    /// The REPO's actions.json (deterministic path from the manifest dir, independent of where the
    /// test binary lands): src-tauri/../actions.json.
    fn repo_actions_json() -> Value {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("actions.json");
        let text = std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("actions.json missing at {} — {e}", p.display()));
        serde_json::from_str(&text).expect("actions.json is not valid JSON")
    }

    // ---------------- THE CLOSURE CONTRACT (failure catalog #3) ----------------
    // Every diagnosis supervisor::diagnose can emit maps to an executable remediation, and every
    // mapped action/fallback is in the closed kind set. This test IS the "no detection without
    // actuation" guarantee: removing a category mapping (or typo-ing a kind) fails cargo test —
    // the build gate — so an unmapped diagnosis can never ship.
    #[test]
    fn closure_every_diagnosis_maps_to_a_closed_executable_action() {
        let doc = repo_actions_json();
        assert_eq!(doc["version"], serde_json::json!(1), "actions.json must carry version 1");
        assert!(
            doc["default_ttl_s"].as_u64().unwrap_or(0) > 0,
            "actions.json needs a positive default_ttl_s"
        );
        let diagnoses = doc["diagnoses"]
            .as_object()
            .expect("actions.json needs a 'diagnoses' object");

        // (a) TOTAL over diagnose()'s emitted categories.
        for cat in crate::supervisor::diagnose_categories() {
            let row = diagnoses.get(*cat).unwrap_or_else(|| {
                panic!(
                    "actions.json has NO mapping for diagnosis category '{cat}' — every \
                     diagnosis must map to an executable remediation (failure catalog #3); \
                     add a {{action, ttl_s, fallback, max_fallback_runs}} entry for it"
                )
            });
            assert!(
                row.get("ttl_s").and_then(Value::as_u64).unwrap_or(0) > 0,
                "'{cat}' needs a positive integer ttl_s"
            );
            assert!(
                row.get("max_fallback_runs").and_then(Value::as_u64).is_some(),
                "'{cat}' needs an integer max_fallback_runs"
            );
        }

        // (b) CLOSED over every row in the file (extra rows like running_stalled included).
        for (cat, row) in diagnoses {
            for field in ["action", "fallback"] {
                let kind = row.get(field).and_then(Value::as_str).unwrap_or_else(|| {
                    panic!("'{cat}'.{field} must be a string action kind")
                });
                assert!(
                    ACTION_KINDS.contains(&kind),
                    "'{cat}'.{field} = '{kind}' is NOT in the closed action kind set \
                     {ACTION_KINDS:?} — only executable, auto-safe kinds are allowed"
                );
            }
        }
    }

    // ---------------- policy lookup ----------------
    #[test]
    fn policy_from_reads_row_and_defaults_unknown_categories_to_paged_ttl() {
        let doc = serde_json::json!({
            "version": 1,
            "default_ttl_s": 1234,
            "diagnoses": {
                "quota_error": {"action": "park_primary_endpoint", "ttl_s": 60,
                                 "fallback": "park_primary_endpoint", "max_fallback_runs": 2}
            }
        });
        let p = policy_from(&doc, "quota_error");
        assert_eq!(p.action, "park_primary_endpoint");
        assert_eq!(p.ttl_s, 60);
        assert_eq!(p.fallback, "park_primary_endpoint");
        assert_eq!(p.max_fallback_runs, 2);

        // unknown category: default ttl + deduped paging — never a panic, never a dead end.
        let u = policy_from(&doc, "martian_weather");
        assert_eq!(u.ttl_s, 1234);
        assert_eq!(u.fallback, DEGRADED_KIND);
        assert_eq!(u.action, DEGRADED_KIND);

        // corrupt/empty doc: the hardcoded floor holds (no 0-ttl fallback storms).
        let e = policy_from(&serde_json::json!({}), "anything");
        assert_eq!(e.ttl_s, DEFAULT_TTL_S);
        assert_eq!(e.fallback, DEGRADED_KIND);

        // ttl_s: 0 in a row is refused in favor of the default (a 0 TTL = execute every sweep).
        let z = policy_from(
            &serde_json::json!({"default_ttl_s": 500, "diagnoses": {"x": {"action": "none", "ttl_s": 0, "fallback": "none", "max_fallback_runs": 1}}}),
            "x",
        );
        assert_eq!(z.ttl_s, 500);
    }

    // ---------------- execute_action totality ----------------
    #[test]
    fn execute_action_none_and_unknown_kinds_never_panic() {
        let repo = serde_json::json!({"name": "actions_none_test"});
        let ok = execute_action("none", &repo, "ok", false);
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["kind"], "none");

        // an unknown kind (data-file typo) is an honest refusal, not a panic and not a no-op lie.
        let bad = execute_action("summon_operator_telepathically", &repo, "x", false);
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].as_str().unwrap().contains("closed registry"));

        // park on a nameless repo row refuses instead of parking a garbage ledger key.
        let park = execute_action("park_primary_endpoint", &serde_json::json!({}), "quota_error", false);
        assert_eq!(park["ok"], false);
    }

    // ---------------- page marker dedup ----------------
    #[test]
    fn page_operator_dedup_marker_suppresses_within_24h_and_reopens_after() {
        // Serialize against every other test that flips the notify kill-switch env var.
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let name = format!("actions_page_test_{}", std::process::id());
        let repo = serde_json::json!({ "name": name });
        let dir = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        // 1st page: sent (kill-switched, but the marker is stamped).
        let r1 = execute_action("page_operator_deduped", &repo, "no_key", false);
        assert_eq!(r1["ok"], true);
        assert_eq!(r1["deduped"], false);
        assert!(page_marker_path(&dir, "no_key").exists(), "marker stamped");

        // 2nd page within the window: deduped.
        let r2 = execute_action("page_operator_deduped", &repo, "no_key", false);
        assert_eq!(r2["deduped"], true);

        // a DIFFERENT category has its own marker — not suppressed by no_key's page.
        let r3 = execute_action("page_operator_deduped", &repo, "stuck", false);
        assert_eq!(r3["deduped"], false);

        // age the no_key marker past 24h: pages again (once per day, not once ever).
        std::fs::write(
            page_marker_path(&dir, "no_key"),
            format!("{}", unix_now() - PAGE_DEDUP_WINDOW_S - 1),
        )
        .unwrap();
        let r4 = execute_action("page_operator_deduped", &repo, "no_key", false);
        assert_eq!(r4["deduped"], false);

        // unparseable marker content reads as due (page + heal the marker), never a panic.
        std::fs::write(page_marker_path(&dir, "no_key"), "garbage").unwrap();
        assert!(!page_deduped_at(&dir, "no_key", unix_now()));

        // category sanitization: a hostile category cannot escape the runtime dir.
        let m = page_marker_path(&dir, "../../evil");
        assert!(m.starts_with(&dir));
        assert!(!m.to_string_lossy().contains(".."));

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- doc loading resilience ----------------
    #[test]
    fn load_doc_from_missing_or_corrupt_file_degrades_to_empty_object() {
        let dir = std::env::temp_dir().join(format!("solomon_actions_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(load_doc_from(&dir.join("nope.json")), serde_json::json!({}));
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert_eq!(load_doc_from(&bad), serde_json::json!({}));
        let arr = dir.join("arr.json");
        std::fs::write(&arr, "[1,2,3]").unwrap();
        assert_eq!(load_doc_from(&arr), serde_json::json!({}), "non-object degrades too");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
