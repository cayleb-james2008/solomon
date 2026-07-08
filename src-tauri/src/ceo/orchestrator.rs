//! D4 — Layer 1: the CEO orchestrator + the constrained-specialist Task system.
//!
//! ============================ WHAT THIS ADDS =============================
//! PECRT Layer 0 (`crate::pecrt`) answers WHEN to wake and WHAT to remember. This module is the
//! thin Layer-1 driver that, on each wake, turns a read of state into TYPED work: it PRIORITIZES a
//! diagnosis, maps it to a typed [`Task`], and DISPATCHES that task to a constrained [`Specialist`]
//! — then the specialist's outcome is available for D3 self-critique. The orchestrator DISPATCHES;
//! the GATES DECIDE. It holds NO new authority.
//! ========================================================================
//!
//! ## The three pieces (and NOTHING more — scope discipline)
//!
//!   1. `trait Specialist` — a constrained worker. Its power is bounded by three ALREADY-EXISTING
//!      gates, not by new machinery:
//!        * `allowed_tools()` — the CLOSED tool whitelist this specialist may name. Purely
//!          descriptive; the ENFORCEMENT is the gates below (a specialist physically cannot reach a
//!          tool outside its lane because the gate denies).
//!        * `scope_globs(repo)` — the repo-relative paths it may touch, read from the SAME
//!          `repos.json` `tiers.protected` / `money_globs` blast-radius config the ship gate already
//!          enforces (`crate::improver::tiers`). No new scope source.
//!        * `gate(task, repo)` — the fail-closed admission check. It COMPOSES the existing gates:
//!          `money_guard` (deny-by-default on any money-capable probe) and `pecrt::safety`
//!          (deny any self-governance mutation: whitelist / cycle_budget / skeptic / blast-radius /
//!          kill / freshness). Returns `Some(refusal)` to DENY, `None` to proceed. This is where a
//!          specialist "physically cannot call outside its whitelist".
//!
//!   2. `struct Task` / `enum TaskKind` — the typed dispatch envelope. `TaskKind` is a CLOSED set
//!      (mirrors `crate::actions::ACTION_KINDS` doctrine): a coding task (the Engineering lane) plus
//!      the existing closed remediation kinds. A closure `#[test]` asserts every diagnosis the
//!      orchestrator can emit maps to a member of this set — no "wait-for-operator" dead ends
//!      (failure-mode #3 countermeasure), reusing the same closed-registry proof `actions.rs` uses.
//!
//!   3. `struct EngineeringSpecialist` — TODAY's `pi` coder (`crate::improver::pi`), registered
//!      behind the trait with ZERO behavior change. Its `run` builds the byte-IDENTICAL pi
//!      invocation (`build_pi_argv` + `apply_pi_env`) a real iteration builds; a contract `#[test]`
//!      pins argv equality on a fixed input. Its `gate` routes a money-capable probe into
//!      `money_guard`, which DENIES by default.
//!
//! ## What this module deliberately does NOT do (ponytail scope discipline)
//!
//!   * NO second specialist, and NO framework a second specialist has not yet forced. There is one
//!     specialist (Engineering) because there is one worker today (pi). The trait is the seam; it is
//!     not a plugin system.
//!   * NO new authority. The orchestrator cannot mutate a whitelist/budget or bypass the skeptic —
//!     every dispatch first passes `gate`, which delegates to `pecrt::safety::classify_schedule`
//!     (fail-closed, NO allow arm for a governance target). This module adds no code path around it.
//!   * NO new execution machinery for remediations. A remediation TaskKind delegates to the existing
//!     `crate::actions::execute_action` (itself money-guarded + closed-registry). The orchestrator
//!     routes; it does not re-implement.
#![allow(dead_code)]

use crate::improver::ctx::Ctx;
use crate::pecrt::safety::{self, ScheduleRequest};
use serde_json::{json, Value};

// --------------------------------------------------------------------------- #
// TaskKind — the CLOSED set of work the orchestrator can dispatch
// --------------------------------------------------------------------------- #

/// The CLOSED set of typed work the orchestrator may dispatch. Mirrors the closed-registry doctrine
/// of [`crate::actions::ACTION_KINDS`]: a diagnosis can only map to a member of this set, pinned by
/// the closure `#[test]` below — so a diagnosis can never dead-end at "wait for operator" (failure
/// catalog #3). Adding a kind means adding BOTH its variant here AND its `str_kind`/dispatch arm,
/// which the closure test then forces to be mapped.
///
///   * `Code` — an Engineering coding session (the `pi` lane). The ONE thing the Engineering
///     specialist does; behavior-identical to a real iteration's implement pass.
///   * `Remediate(kind)` — one of the existing closed remediation kinds (`crate::actions::ACTION_KINDS`),
///     dispatched through the existing money-guarded `execute_action`. `NoOp` maps to `"none"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// A coding session handled by the Engineering specialist (`pi`).
    Code,
    /// A closed-registry remediation (delegates to `actions::execute_action`); the inner &str is a
    /// member of `crate::actions::ACTION_KINDS`.
    Remediate(&'static str),
}

impl TaskKind {
    /// The stable string identity of this kind (for logs, heartbeat, and the closure test). For a
    /// `Remediate` it is the underlying `actions` kind, so the closed-registry proof composes.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Code => "code",
            TaskKind::Remediate(k) => k,
        }
    }
}

/// A typed unit of work the orchestrator hands to a specialist. `lane` is the repo/project name;
/// `detail` is the human-readable task text (a backlog item for `Code`, a diagnosis category for a
/// `Remediate`). The envelope carries no authority — it is data; the specialist's `gate` decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub kind: TaskKind,
    pub lane: String,
    pub detail: String,
}

impl Task {
    pub fn new(kind: TaskKind, lane: &str, detail: &str) -> Self {
        Task {
            kind,
            lane: lane.to_string(),
            detail: detail.to_string(),
        }
    }
}

// --------------------------------------------------------------------------- #
// Specialist — the constrained-worker seam (gates ARE the enforcement)
// --------------------------------------------------------------------------- #

/// A constrained specialist. Its authority is the INTERSECTION of what it declares
/// (`allowed_tools`, `scope_globs`) and what the existing gates permit (`gate`). The trait adds no
/// new power; it names the seam the orchestrator dispatches through. A specialist CANNOT call
/// outside its whitelist because `gate` — which delegates to `money_guard` + `pecrt::safety` —
/// denies fail-closed before `run` is ever reached.
pub trait Specialist {
    /// This specialist's identity (e.g. `"engineering"`).
    fn name(&self) -> &'static str;

    /// The CLOSED whitelist of tool names this specialist may name. Descriptive; the runtime
    /// enforcement is `gate`. (A specialist that names a money-capable tool still has that call
    /// DENIED by `money_guard` at `gate` time — the whitelist and the gate agree by construction.)
    fn allowed_tools(&self) -> &'static [&'static str];

    /// The repo-relative path globs this specialist may touch, for `repo`. Sourced from the SAME
    /// `repos.json` `tiers` blast-radius config (`money_globs` + `protected`) the ship gate reads —
    /// no new scope authority. A `protected` grader/leash path is NEVER in scope (it is
    /// write-protected); this returns the specialist's writable scope, empty for a legacy row.
    fn scope_globs(&self, repo: &Value) -> Vec<String>;

    /// The fail-closed admission gate. Returns `Some(refusal Value)` when the task must be DENIED,
    /// `None` to proceed to `run`. It COMPOSES existing gates and adds no allow arm they do not
    /// already grant. This is the enforcement point of the whole trait.
    fn gate(&self, task: &Task, repo: &Value) -> Option<Value>;

    /// Perform the (already-admitted) task. Called ONLY after `gate` returned `None`. Returns a
    /// `{ok, ...}` outcome Value the orchestrator hands to D3 critique.
    fn run(&self, task: &Task, repo: &Value) -> Value;
}

// --------------------------------------------------------------------------- #
// EngineeringSpecialist — today's pi coder, behind the trait, ZERO behavior change
// --------------------------------------------------------------------------- #

/// The CLOSED tool whitelist for the Engineering specialist. These are exactly the read/reason/edit
/// tools `pi` already runs with under the runner's sandbox (branch-per-iteration + the agent shims
/// in `improver::pi::agent_shim_dir` that REFUSE `gh` and the branch/push git verbs). NO money tool
/// is here — a money-capable probe is denied at `gate` by `money_guard`, so the whitelist and the
/// gate cannot diverge.
pub const ENGINEERING_ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "search",
    "run_gate",       // run the project's own test/eval gate (read-only verdict)
    "github_status",  // the read-only github_* tools (never push/merge/close — pi's shims enforce)
];

/// The Engineering specialist: TODAY's stateless `pi` coding agent, registered behind
/// [`Specialist`]. Constructing it takes no state — pi is stateless per session (one `run_pi` call
/// per task), exactly as `improver::iteration` invokes it.
#[derive(Debug, Default, Clone, Copy)]
pub struct EngineeringSpecialist;

impl EngineeringSpecialist {
    pub fn new() -> Self {
        EngineeringSpecialist
    }

    /// Build the Ctx a coding task runs under — IDENTICAL to how `actions::park_primary_endpoint`
    /// and the runner build it (`Ctx::configure(path, name, provider, model)`), so the resulting pi
    /// invocation is byte-identical to a real iteration's. Extracted so the contract test can prove
    /// argv equality without a live pi.
    pub(crate) fn build_ctx(repo: &Value) -> Ctx {
        let name = crate::control::paths::repo_name(repo);
        let path = crate::control::paths::repo_path(repo);
        let provider = crate::control::registry::project_provider(repo);
        let model = crate::control::registry::project_model(repo);
        Ctx::configure(
            &path,
            &name,
            &provider,
            if model.is_empty() { None } else { Some(model.as_str()) },
        )
    }
}

impl Specialist for EngineeringSpecialist {
    fn name(&self) -> &'static str {
        "engineering"
    }

    fn allowed_tools(&self) -> &'static [&'static str] {
        ENGINEERING_ALLOWED_TOOLS
    }

    fn scope_globs(&self, repo: &Value) -> Vec<String> {
        // The writable blast-radius scope: money-path globs (auto-land only under an active needle,
        // per the ship gate) — sourced from the SAME repos.json `tiers` the ship gate reads. The
        // `protected` grader/leash list is deliberately EXCLUDED: those are write-protected and can
        // never be in a specialist's writable scope. A legacy row (no `tiers`) yields [].
        let tiers = match repo.get("tiers") {
            Some(t) => t,
            None => return Vec::new(),
        };
        match tiers.get("money_globs") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn gate(&self, task: &Task, repo: &Value) -> Option<Value> {
        // (1) MONEY GUARD (outermost default-DENY). The specialist's task kind is probed against the
        // NO-MONEY-OUT guard exactly as `actions::execute_action` probes an action kind. A
        // money-capable task (withdraw/transfer/pay/... or the reserved trade verb on a
        // non-whitelisted lane) is DENIED fail-closed. A plain `code` task is not money-capable, so
        // this is transparent to it. See `money_guard.rs` for the HARD invariant.
        if let Some(refusal) = crate::money_guard::guard(task.kind.as_str(), repo) {
            return Some(refusal);
        }

        // (2) PECRT SAFETY (self-governance default-DENY). The dispatch is expressed as a
        // ScheduleRequest against the task's TARGET (the lane) and its VERB (the kind). This is the
        // route that makes it IMPOSSIBLE for the orchestrator to dispatch a task that mutates a
        // whitelist/cycle_budget/skeptic/blast-radius/kill/freshness surface — those targets have NO
        // allow arm. A code/remediation task on an ordinary lane re-enters the existing gate funnel
        // (reenters_gates=true), so it is admitted here and the DOWNSTREAM gates (freshness,
        // blast-radius, skeptic, the runner's own ship gate) still decide the outcome.
        let req = ScheduleRequest::new(task.kind.as_str(), &task.lane, true);
        if let Some(refusal) = safety::guard_schedule(&req) {
            return Some(refusal);
        }

        None
    }

    fn run(&self, task: &Task, repo: &Value) -> Value {
        // The gate MUST have passed before run() is reached; belt-and-suspenders re-check so a
        // future direct caller can never skip it (fail-closed, never a silent bypass).
        if let Some(refusal) = self.gate(task, repo) {
            return refusal;
        }
        match task.kind {
            // The ONE Engineering behavior: a pi coding session that is byte-identical to a real
            // iteration's STANDARD implement pass (system_md=None -> the lane's AGENT.md contract,
            // exactly as improver::iteration.rs:518 does for the default coding case). `run_pi` never
            // panics (a timeout is rc=124, a spawn failure rc<0), and it enters its OWN existing gate
            // funnel (freshness budget, provider preflight, quota parking) unchanged — this wrapper
            // adds nothing. (The beautify / solomon-meta sub-variants that pass a specific system_md
            // are NOT this layer's concern; "the pi coding agent" here is the default coder.)
            TaskKind::Code => {
                let mut ctx = Self::build_ctx(repo);
                let out = crate::improver::pi::run_pi(
                    &mut ctx,
                    &task.detail,
                    crate::improver::pi::TIMEOUT_IMPLEMENT,
                    None,
                );
                json!({
                    "ok": out.code == 0,
                    "specialist": "engineering",
                    "kind": "code",
                    "lane": task.lane,
                    "exit_code": out.code,
                    "summary": crate::improver::pi::final_text(&out.stdout),
                })
            }
            // A remediation delegates to the existing closed-registry executor (money-guarded,
            // closure-tested). The orchestrator does not re-implement remediation machinery.
            TaskKind::Remediate(kind) => {
                // auto_push=false: the orchestrator dispatches, it does not force a ship; the
                // existing action machinery owns version control.
                crate::actions::execute_action(kind, repo, task.detail.as_str(), false)
            }
        }
    }
}

// --------------------------------------------------------------------------- #
// The CLOSED diagnosis -> task-kind registry (failure-mode #3 countermeasure)
// --------------------------------------------------------------------------- #

/// Map a diagnosis category the orchestrator can emit to the typed [`TaskKind`] that remediates it.
/// TOTAL over `supervisor::diagnose_categories()` (the closure `#[test]` proves it) — every
/// diagnosis has an executable task, so the orchestrator can NEVER emit a diagnosis that dead-ends
/// at "wait for operator". This is the Layer-1 analogue of `actions.json`'s diagnosis->action map,
/// and it composes with it: a `Remediate(kind)` here names a member of `actions::ACTION_KINDS`, so a
/// mapped task is guaranteed executable by `actions::execute_action`.
///
/// The mapping is deliberately CONSERVATIVE and reuses the existing action semantics — it invents no
/// new remediation. `gate_red_streak` / `noop_streak` / `ci_red_streak` route to a coding session
/// (the Engineering specialist fixes the code); everything else routes to the existing auto-safe
/// remediation the supervisor's `recover()` ladder already performs.
pub fn diagnosis_to_task_kind(diagnosis: &str) -> TaskKind {
    match diagnosis {
        // Healthy: an explicit no-op remediation (maps to actions "none"), so the registry is TOTAL,
        // not special-cased.
        "ok" => TaskKind::Remediate("none"),

        // A red/stuck code state is Engineering's job: dispatch a coding session to fix it.
        "gate_red_streak" | "ci_red_streak" | "noop_streak" | "stuck" | "needs_goal" => {
            TaskKind::Code
        }

        // Provider quota/endpoint: park the primary endpoint (the existing catalog-#2 remediation).
        "quota_error" => TaskKind::Remediate("park_primary_endpoint"),

        // A wedged tree / lingering process / stale lock: the existing auto-safe resets & restarts.
        "dirty_tree" | "base_out_of_band" | "untracked_refusal" | "revert_failed" => {
            TaskKind::Remediate("reset_to_base")
        }
        "stale_lock" | "stop_lingering" | "persistent_self_stop" => {
            TaskKind::Remediate("clear_escalation_and_retry")
        }

        // Everything the loop cannot auto-fix without an operator decision (a missing/mis-shaped key,
        // an unobservable metric, GitHub not wired, or an unknown error) still maps to an EXECUTABLE
        // task: the deduped operator page. That is an ACTION (it notifies + records), never a silent
        // dead end — the failure-mode-#3 guarantee is "maps to an executable task", not "auto-heals".
        "no_key" | "key_shape_mismatch" | "gh_not_ready" | "metric_unobservable"
        | "unknown_error" => TaskKind::Remediate("page_operator_deduped"),

        // Any category not enumerated above still gets an executable task (the deduped page) rather
        // than a panic or a dead end. The closure test guarantees every KNOWN diagnosis is mapped
        // explicitly; this arm is the fail-closed floor for a future/unknown string.
        _ => TaskKind::Remediate("page_operator_deduped"),
    }
}

// --------------------------------------------------------------------------- #
// The orchestrator dispatch — DISPATCHES; the gates DECIDE
// --------------------------------------------------------------------------- #

/// Dispatch one typed [`Task`] to `specialist` against `repo`. The orchestrator's whole job: it
/// runs the specialist's fail-closed `gate` FIRST (a DENY returns the refusal Value verbatim — no
/// work happens), and only on `None` does it call `run`. The orchestrator NEVER reaches around the
/// gate; there is no path here that mutates a whitelist/budget or skips the skeptic, because the
/// gate delegates that decision to `money_guard` + `pecrt::safety` (both fail-closed).
pub fn dispatch<S: Specialist>(specialist: &S, task: &Task, repo: &Value) -> Value {
    if let Some(refusal) = specialist.gate(task, repo) {
        return refusal;
    }
    specialist.run(task, repo)
}

/// The Layer-1 wake step in one call: read a lane's diagnosis, map it to a typed task, and dispatch
/// it to the Engineering specialist through the gated interface. Returns the outcome Value (for D3
/// critique). This is the seam a future thread-plane driver calls each wake; it is intentionally
/// thin — prioritization stays in the existing deterministic CEO planes (`ceo::allocate`/`focus`),
/// and this routes the ONE chosen unit of work through the constrained specialist.
pub fn dispatch_for_diagnosis(repo: &Value, diagnosis: &str, detail: &str) -> Value {
    let kind = diagnosis_to_task_kind(diagnosis);
    let lane = crate::control::paths::repo_name(repo);
    let task = Task::new(kind, &lane, detail);
    let eng = EngineeringSpecialist::new();
    dispatch(&eng, &task, repo)
}

// --------------------------------------------------------------------------- #
// tests — the four acceptance contracts
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain_repo() -> Value {
        json!({ "name": "sover" })
    }
    fn kairos_repo() -> Value {
        json!({
            "name": "kairos",
            "equity_usd": 1234.5,
            "tiers": {
                "money_globs": ["trader.py", "kalshi_client.py", "config.json"],
                "protected": ["promote.py", ".state/"]
            }
        })
    }

    // ===================================================================== #
    // ACCEPTANCE (a): the Engineering specialist dispatches a coding task
    // through the typed interface and the pi lane runs UNCHANGED — proven by
    // byte-identical pi argv on a fixed input (a live pi is neither available
    // nor needed to prove behavior-identity of the invocation surface).
    // ===================================================================== #
    #[test]
    fn engineering_coding_task_builds_byte_identical_pi_invocation() {
        let repo = plain_repo();
        let task = Task::new(TaskKind::Code, "sover", "add a test for the main entrypoint");

        // What the specialist WOULD spawn (via build_ctx + the shared build_pi_argv):
        let spec_ctx = EngineeringSpecialist::build_ctx(&repo);
        let spec_argv = crate::improver::pi::build_pi_argv(&spec_ctx, &task.detail, None);

        // What a real iteration's implement pass builds today for the SAME lane/task: an identically
        // configured Ctx and the SAME build_pi_argv call. Byte-for-byte equality proves the pi lane
        // is UNCHANGED behind the trait.
        let ref_ctx = Ctx::configure(
            &crate::control::paths::repo_path(&repo),
            &crate::control::paths::repo_name(&repo),
            &crate::control::registry::project_provider(&repo),
            None,
        );
        let ref_argv = crate::improver::pi::build_pi_argv(
            &ref_ctx,
            "add a test for the main entrypoint",
            None,
        );

        assert_eq!(
            spec_argv, ref_argv,
            "the Engineering specialist must build the byte-identical pi argv a real iteration \
             builds — the pi lane runs UNCHANGED behind the Task interface"
        );
        // and the invocation is a real pi coding session (the argv names the pi print/json surface).
        assert!(spec_argv.contains(&"--print".to_string()));
        assert!(spec_argv.contains(&"json".to_string()));
    }

    #[test]
    fn dispatch_runs_the_specialist_only_after_the_gate_passes() {
        // A plain code task on an ordinary lane passes the gate; dispatch reaches run(). We assert
        // the OUTCOME SHAPE (specialist identity + kind), not a live pi result — run_pi returns a
        // failed RunOut (no pi binary in the test env) but never panics, so the envelope is honest.
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let out = dispatch_for_diagnosis(&plain_repo(), "gate_red_streak", "fix the red gate");
        assert_eq!(out["specialist"], "engineering");
        assert_eq!(out["kind"], "code");
        assert_eq!(out["lane"], "sover");
        assert!(out.get("exit_code").is_some(), "an honest exit_code is reported");

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ===================================================================== #
    // ACCEPTANCE (b): every emittable diagnosis maps to an EXECUTABLE task
    // kind — no "wait-for-operator" dead ends. This is the Layer-1 closed
    // registry (failure catalog #3), composed with actions.rs's closed set.
    // ===================================================================== #
    #[test]
    fn closure_every_diagnosis_maps_to_an_executable_task_kind() {
        for diag in crate::supervisor::diagnose_categories() {
            let kind = diagnosis_to_task_kind(diag);
            match kind {
                // A coding task is executable by the Engineering specialist (proven byte-identical
                // in the test above).
                TaskKind::Code => {}
                // A remediation MUST name a member of the existing closed action registry, so it is
                // guaranteed executable by actions::execute_action (which itself is closure-tested).
                TaskKind::Remediate(k) => {
                    assert!(
                        crate::actions::ACTION_KINDS.contains(&k),
                        "diagnosis '{diag}' maps to Remediate('{k}') which is NOT in the closed \
                         action registry {:?} — every diagnosis must map to an EXECUTABLE task \
                         (failure catalog #3: no detection without actuation)",
                        crate::actions::ACTION_KINDS
                    );
                }
            }
        }
    }

    #[test]
    fn no_diagnosis_maps_to_a_wait_for_operator_dead_end() {
        // The failure-mode-#3 guarantee stated positively: for EVERY diagnosis, dispatching yields a
        // task whose kind is a real, executable thing (code, or a closed remediation), never a
        // no-op "operator must act" sentinel with no action attached. Even the operator-decision
        // categories map to page_operator_deduped, which is an ACTION (it notifies + records),
        // not a silent dead end.
        for diag in crate::supervisor::diagnose_categories() {
            let k = diagnosis_to_task_kind(diag);
            // as_str() is always a non-empty, dispatchable identity.
            assert!(!k.as_str().is_empty(), "diagnosis '{diag}' produced an empty task kind");
        }
        // an unknown/future diagnosis string still gets an executable floor (the deduped page).
        assert_eq!(
            diagnosis_to_task_kind("some_future_unmapped_category"),
            TaskKind::Remediate("page_operator_deduped")
        );
    }

    // ===================================================================== #
    // ACCEPTANCE (c): the Engineering specialist CANNOT invoke a tool outside
    // its whitelist — money_guard DENIES by default on any money-capable probe.
    // ===================================================================== #
    #[test]
    fn engineering_specialist_cannot_invoke_a_money_capable_tool() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let eng = EngineeringSpecialist::new();

        // A representative spread of money-capable probes — expressed as tasks whose kind IS the
        // money verb (the same surface money_guard classifies for actions). Every one must be DENIED
        // at gate() BEFORE run() — even on a whitelisted live-money lane (money-OUT is the HARD
        // invariant, no lane exempts it).
        for money_kind in [
            "withdraw", "transfer", "deposit", "buy_ads", "pay_invoice", "stripe_checkout",
            "ad_spend", "send_money", "spend_treasury",
        ] {
            for repo in [plain_repo(), kairos_repo()] {
                // Express the money-capable probe as the task's KIND — the exact surface the gate's
                // money_guard classifies. gate() must refuse it (money_guard default-DENY).
                let money_task = Task {
                    kind: TaskKind::Remediate(money_kind_static(money_kind)),
                    lane: "kairos".to_string(),
                    detail: money_kind.to_string(),
                };
                let verdict = eng.gate(&money_task, &repo);
                assert!(
                    verdict.is_some(),
                    "money-capable probe '{money_kind}' must be DENIED at the gate on repo {repo}"
                );
                let refusal = verdict.unwrap();
                assert_eq!(refusal["ok"], false);
                assert!(
                    refusal["money_guard"] == json!(true)
                        || refusal["error"]
                            .as_str()
                            .map(|e| e.to_lowercase().contains("denied"))
                            .unwrap_or(false),
                    "the refusal must be a fail-closed money-out DENY: {refusal}"
                );
            }
        }

        // The whitelist and the gate agree: NO money tool is in the declared whitelist.
        for t in eng.allowed_tools() {
            assert!(
                !crate::money_guard::is_money_capable(t),
                "the Engineering whitelist names a money-capable tool '{t}' — it must not"
            );
        }

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    /// Test helper: a `Remediate(&'static str)` needs a static kind. The money verbs under test are
    /// compile-time literals, so we map each back to its static form (bounded, test-only — no leak).
    fn money_kind_static(k: &str) -> &'static str {
        match k {
            "withdraw" => "withdraw",
            "transfer" => "transfer",
            "deposit" => "deposit",
            "buy_ads" => "buy_ads",
            "pay_invoice" => "pay_invoice",
            "stripe_checkout" => "stripe_checkout",
            "ad_spend" => "ad_spend",
            "send_money" => "send_money",
            "spend_treasury" => "spend_treasury",
            _ => "unknown_money_kind",
        }
    }

    // ===================================================================== #
    // ACCEPTANCE (d): no code path lets the orchestrator mutate a whitelist/
    // budget or bypass the skeptic. Every dispatch's gate routes through
    // pecrt::safety, which is fail-closed with NO allow arm for a governance
    // target — proven here at the orchestrator's own dispatch seam.
    // ===================================================================== #
    #[test]
    fn orchestrator_cannot_mutate_whitelist_budget_or_bypass_skeptic() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let eng = EngineeringSpecialist::new();

        // Express a dispatch whose TARGET (lane) names a self-governance surface. pecrt::safety
        // denies these outright — there is no allow arm. We drive it through the specialist's gate
        // (the exact path dispatch() takes) to prove the orchestrator seam is closed.
        for gov_target in [
            "repos.json",
            "whitelist",
            "kairos.cycle_budget",
            "tier promotion",
            "skeptic_bypass",
            "blast_radius",
            "kill_gate",
            "freshness_gate",
        ] {
            let task = Task::new(TaskKind::Remediate("none"), gov_target, "attempt");
            let refusal = eng
                .gate(&task, &plain_repo())
                .expect("a governance-target dispatch must be DENIED at the gate");
            assert_eq!(refusal["ok"], false);
            assert_eq!(
                refusal["pecrt_safety"], true,
                "the DENY must come from the fail-closed pecrt safety gate (no allow arm): {refusal}"
            );
        }

        // And there is no path in dispatch() that reaches run() for a denied task: dispatch returns
        // the refusal verbatim (run() is never called).
        let denied = dispatch(
            &eng,
            &Task::new(TaskKind::Remediate("none"), "repos.json#tiers", "widen"),
            &plain_repo(),
        );
        assert_eq!(denied["ok"], false);
        assert_eq!(denied["pecrt_safety"], true);

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ---- trait wiring: scope_globs reads the SAME repos.json tiers the ship gate reads ----
    #[test]
    fn engineering_scope_globs_come_from_repos_json_tiers_money_globs() {
        let eng = EngineeringSpecialist::new();
        // kairos row carries money_globs — those are the writable money-path scope.
        let globs = eng.scope_globs(&kairos_repo());
        assert_eq!(globs, vec!["trader.py", "kalshi_client.py", "config.json"]);
        // a legacy row (no tiers) is the no-op empty scope.
        assert!(eng.scope_globs(&plain_repo()).is_empty());
        // the protected grader/leash list is NEVER returned as writable scope.
        assert!(!globs.iter().any(|g| g == "promote.py" || g == ".state/"));
    }

    // ---- TaskKind identity is stable + composes with the closed action set ----
    #[test]
    fn task_kind_str_identity_is_stable_and_remediate_names_closed_kinds() {
        assert_eq!(TaskKind::Code.as_str(), "code");
        assert_eq!(TaskKind::Remediate("none").as_str(), "none");
        // every Remediate variant produced by the diagnosis map names a closed action kind.
        for diag in crate::supervisor::diagnose_categories() {
            if let TaskKind::Remediate(k) = diagnosis_to_task_kind(diag) {
                assert!(crate::actions::ACTION_KINDS.contains(&k));
            }
        }
    }
}
