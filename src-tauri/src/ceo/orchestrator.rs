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

use crate::control::proc::RunOut;
use crate::improver::ctx::Ctx;
use crate::improver::{calibration, progress};
use crate::pecrt::safety::{self, ScheduleRequest};
use serde_json::{Value, json};
use std::path::Path;

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
    "run_gate",      // run the project's own test/eval gate (read-only verdict)
    "github_status", // the read-only github_* tools (never push/merge/close — pi's shims enforce)
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
            if model.is_empty() {
                None
            } else {
                Some(model.as_str())
            },
        )
    }

    /// Run a coding session on the caller's ALREADY-CONFIGURED `ctx` — the live-iteration entrypoint
    /// (D10). Unlike [`Specialist::run`], which builds a fresh Ctx for the stateless onboarding/D7
    /// convenience paths, this preserves the live loop's per-cycle ctx mutations (escalation fallback
    /// model, tier reasoning, plan-phase task text, beautify/solomon `system_md`) so the resulting
    /// `pi::run_pi` call is BYTE-IDENTICAL to the pre-D10 inline call it replaces. It is the ONLY
    /// place the live improver reaches `pi::run_pi`, so the dispatch gate (run by
    /// [`dispatch_engineering_on_ctx`]) is the single audited chokepoint.
    ///
    /// This method itself is gate-free by design: it is `pub(crate)` and reached ONLY through
    /// [`dispatch_engineering_on_ctx`], which runs the fail-closed [`Specialist::gate`] FIRST. It
    /// carries NO authority the inline `pi::run_pi` did not — it is the same call, relocated behind
    /// the specialist so the "pi only runs inside a Specialist" invariant holds.
    pub(crate) fn run_pi_on_ctx(
        &self,
        ctx: &mut Ctx,
        task: &str,
        timeout: i64,
        system_md: Option<&Path>,
    ) -> RunOut {
        crate::improver::pi::run_pi(ctx, task, timeout, system_md)
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
        // stranded_unmerged_branch (finding #61) belongs here too: the fix is a HUMAN reconcile of
        // finished work — clear_escalation_and_retry would un-pin the STOP sentinel and thrash
        // park→clear→re-park on a condition that re-trips deterministically every preflight.
        "no_key"
        | "key_shape_mismatch"
        | "gh_not_ready"
        | "metric_unobservable"
        | "stranded_unmerged_branch"
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

/// D10 — the LIVE improver's implement-phase dispatch: route `one_iteration`'s coding session
/// through the SAME fail-closed gate the onboarding path uses, then run `pi` on the caller's live
/// `ctx` so behavior is byte-identical to the inline `pi::run_pi` this replaces.
///
/// This is the seam that closes Layer 1's live half: every improvement now flows through the
/// constrained-Specialist gate (money_guard -> pecrt::safety, run FIRST), not around it. It adds
/// ROUTING, never a bypass:
///
///   * A plain `Code` task on an ordinary engineering lane is NOT money-capable and does NOT name a
///     governance target, so the gate returns `None` and the dispatch proceeds to `pi::run_pi` with
///     the caller's EXACT `task`/`timeout`/`system_md` — the pre-D10 behavior, unchanged. (The
///     downstream freshness/blast-radius/skeptic/KILL gates in `run_pi` and the ship path still
///     decide the outcome; this seam decides nothing they did not already decide.)
///   * A money-capable OR self-governance-mutating task kind is DENIED at the gate BEFORE any pi
///     spawn — returned as `Err(refusal)` so the caller idles it out honestly (no token spent). This
///     is the new chokepoint: a future Growth/Finance specialist selected by task kind would be
///     admitted or refused HERE, at one audited point.
///
/// The `kind` is the closed [`TaskKind`] the caller attributes to this work (`TaskKind::Code` for
/// the standard/beautify/solomon coding pass). `lane` defaults to `ctx.name`. Returns `Ok(RunOut)`
/// on admission (the byte-identical pi result), `Err(refusal Value)` on a gate DENY.
pub fn dispatch_engineering_on_ctx(
    ctx: &mut Ctx,
    repo: &Value,
    kind: TaskKind,
    task: &str,
    timeout: i64,
    system_md: Option<&Path>,
) -> Result<RunOut, Value> {
    let spec = EngineeringSpecialist::new();
    let lane = ctx.name.clone();
    // The dispatch envelope. `detail` is the fully-built task text (the caller has already appended
    // the plan/calibration/value-focus directives, exactly as before) — the gate never inspects the
    // detail, only the (kind, lane) pair, so passing the whole task is safe and keeps the pi call
    // byte-identical.
    let envelope = Task::new(kind, &lane, task);
    // GATE FIRST — fail-closed. A DENY short-circuits with NO pi spawn (the money-out / governance
    // HARD invariant). This is the SAME gate `dispatch()` and the onboarding path run.
    if let Some(refusal) = spec.gate(&envelope, repo) {
        return Err(refusal);
    }
    // Admitted: run pi on the LIVE ctx (byte-identical to the inline call). `pi::run_pi` is reached
    // ONLY here, inside the specialist — the "pi runs only inside a Specialist" invariant.
    Ok(spec.run_pi_on_ctx(ctx, task, timeout, system_md))
}

// --------------------------------------------------------------------------- #
// D7 — anti-retry-theater + task-size calibration RE-ASSERTED at the dispatch decision
// --------------------------------------------------------------------------- #
//
// The two substrates that killed the OLD single-agent loop's retry-theater
// (`improver::progress` — the 3-strike QUARANTINE ledger, catalog #5) and its problem/executor
// mismatch (`improver::calibration` — the per-model ship-rate table, catalog #7) are RE-ASSERTED
// here so the D4 orchestrator layer cannot silently re-introduce either failure. They are wired
// AROUND the specialist's fail-closed `gate` — never through it: quarantine only SKIPS work and
// calibration only SHRINKS it. Neither adds an allow arm, and a gate DENY still short-circuits
// before any strike is recorded (a denied task did no work, so it accrues no strike).
//
// HONESTY (the moat): the orchestrator's `run` NEVER lands a ship (the runner owns version
// control), so it MUST NOT pass "shipped" to `progress::record_outcome`. It passes a non-shipped
// word ("noop" when the specialist ran, "error" when it failed / was denied-with-work) and lets
// `progress::state_hash` decide whether any OBSERVABLE state actually moved. A synthetic no-delta
// task therefore accrues real strikes; a genuinely state-mutating dispatch resets the counter.
// No fabricated progress is possible here.

/// The size_class a Code task carries when the caller has no backlog tier to attribute (the thin
/// `dispatch_for_diagnosis` convenience). "chore" is the SMALLEST-change tier (the same safe
/// default `ceo::plan_items` degrades an unknown tier to) — never a fabricated large class.
const DEFAULT_CODE_SIZE_CLASS: &str = "chore";

/// The CLOSED registry of on-disk ledgers the orchestrator's Task-dispatch decision READS. Every
/// entry MUST have a reader wired into the selection path below (quarantine consults `progress.json`
/// via [`progress::quarantined`]; size-gating consults `_task_calibration.json` via
/// [`calibration::decompose_directive`]). A write-only ledger is a bug (failure catalog #5); the
/// `contracts.rs` startup contract test `every_orchestrator_ledger_is_read_back_into_the_dispatch_decision`
/// asserts each registered ledger is read back to CHANGE the decision. Adding a ledger the
/// orchestrator writes REQUIRES registering it here AND wiring its reader, or that test fails.
pub const ORCHESTRATOR_LEDGERS: &[&str] = &["progress.json", "_task_calibration.json"];

/// Completeness half of the "no write-only ledger" contract (catalog #5): for EVERY ledger in
/// [`ORCHESTRATOR_LEDGERS`], seed it to a value that MUST change the dispatch decision, run the
/// orchestrator's REAL reader over `ctx`, and confirm the reader consulted it. Returns the names of
/// any ledgers whose reader did NOT read the seeded value back (i.e. write-only ledgers) — an empty
/// vec means every ledger is wired. Driven by the `contracts.rs` startup contract test.
///
/// This is deliberately behavioral, not a string grep: it proves the value flows into the decision.
pub fn selection_readers_contract(ctx: &mut Ctx) -> Vec<&'static str> {
    let mut unread: Vec<&'static str> = Vec::new();
    for &ledger in ORCHESTRATOR_LEDGERS {
        let wired = match ledger {
            // progress.json: seed a quarantine, then the reader (`quarantined`) must read it true.
            "progress.json" => {
                let key = progress::selection_key(ctx, "__contract_probe_task__");
                progress::note_selected(ctx, &key, "__contract_probe_task__");
                let pre = progress::state_hash(ctx);
                for _ in 0..progress::QUARANTINE_STRIKES {
                    progress::record_outcome(ctx, &key, &pre, "noop");
                }
                progress::quarantined(ctx, &key)
            }
            // _task_calibration.json: seed a proven-low cell, then the reader
            // (`decompose_directive`) must read it back as Some(directive).
            "_task_calibration.json" => {
                let fleet_dir = ctx
                    .runtime
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| ctx.control.join("runtime"));
                for _ in 0..calibration::MIN_ATTEMPTS {
                    calibration::record_outcome_at(
                        &fleet_dir,
                        &ctx.pi_model,
                        "__contract_class__",
                        false,
                    );
                }
                calibration::decompose_directive(ctx, "__contract_class__").is_some()
            }
            // An unregistered/unknown ledger has no proven reader — report it as write-only.
            _ => false,
        };
        if !wired {
            unread.push(ledger);
        }
    }
    unread
}

/// One candidate unit of work the orchestrator may pick on a wake: a `(diagnosis, detail,
/// size_class)` triple. `size_class` is the backlog tier the live driver already read for a Code
/// item (chore/feature/refactor/architecture) — the class `calibration` gates the SIZE of.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub diagnosis: String,
    pub detail: String,
    pub size_class: String,
}

impl Candidate {
    pub fn new(diagnosis: &str, detail: &str, size_class: &str) -> Self {
        Candidate {
            diagnosis: diagnosis.to_string(),
            detail: detail.to_string(),
            size_class: size_class.to_string(),
        }
    }
}

/// The Layer-1 wake step in one call: read a lane's diagnosis, map it to a typed task, and dispatch
/// it to the Engineering specialist through the gated interface. Returns the outcome Value (for D3
/// critique). This is the seam a future thread-plane driver calls each wake; it is intentionally
/// thin — prioritization stays in the existing deterministic CEO planes (`ceo::allocate`/`focus`),
/// and this routes the ONE chosen unit of work through the constrained specialist.
///
/// D7: routes through [`select_and_dispatch`] with a single candidate, so the SAME quarantine +
/// calibration guards cover this convenience entrypoint — there is no dispatch path that skips
/// them. A lone quarantined candidate idles the wake out honestly (no pi spend).
pub fn dispatch_for_diagnosis(repo: &Value, diagnosis: &str, detail: &str) -> Value {
    select_and_dispatch(
        repo,
        &[Candidate::new(diagnosis, detail, DEFAULT_CODE_SIZE_CLASS)],
    )
}

/// D7 — the orchestrator's TASK-SELECTION decision, quarantine- and calibration-guarded.
///
/// Given the lane's ordered candidate work for this wake, the orchestrator MUST NOT re-dispatch a
/// task that already completed [`progress::QUARANTINE_STRIKES`] times with no observable state
/// delta — it picks a DIFFERENT candidate. Concretely, per candidate (in priority order):
///
///   1. QUARANTINE (catalog #5): skip any candidate whose `progress::selection_key` is currently
///      quarantined — the orchestrator is FORCED onto different work. If EVERY candidate is
///      quarantined, dispatch NOTHING and return an honest `all_quarantined` idle (no pi spend),
///      mirroring `progress::filter_quarantined_selection`'s bail.
///   2. CALIBRATION (catalog #7): for the picked Code candidate, gate its SIZE by the assigned
///      model's measured ship-rate — `calibration::note_selection` stamps the pending attempt and
///      `calibration::decompose_directive` appends the MANDATORY decompose directive when the
///      (model, size_class) cell is proven low (>= MIN_ATTEMPTS evidence, ship-rate < floor), so an
///      over-sized item runs as its smallest shippable slice instead of whole-and-failing again.
///   3. DISPATCH through the existing gated [`dispatch`] (money_guard -> pecrt::safety FIRST —
///      unchanged). A gate DENY returns the refusal verbatim and records NO strike (no work
///      happened). Otherwise the terminal outcome rides `progress::record_outcome`, which measures
///      the real state delta AND resolves the calibration pending in one hook (wiring point B).
///
/// Returns the specialist's outcome Value (for D3 critique), or the honest idle Value when all
/// candidates are quarantined. Builds the lane Ctx from `repo`; see [`select_and_dispatch_with_ctx`]
/// for the Ctx-injecting core the tests drive in isolation.
pub fn select_and_dispatch(repo: &Value, candidates: &[Candidate]) -> Value {
    let mut ctx = EngineeringSpecialist::build_ctx(repo);
    select_and_dispatch_with_ctx(&mut ctx, repo, candidates)
}

/// The Ctx-injecting core of [`select_and_dispatch`] — split out so a `#[test]` can drive the
/// quarantine + calibration wiring over an ISOLATED runtime dir (never the live `runtime/`), the
/// same isolation the `progress`/`calibration` unit tests use. Production callers go through
/// [`select_and_dispatch`], which builds the real lane Ctx and dispatches to the Engineering
/// specialist.
pub fn select_and_dispatch_with_ctx(
    ctx: &mut Ctx,
    repo: &Value,
    candidates: &[Candidate],
) -> Value {
    select_and_dispatch_core(&EngineeringSpecialist::new(), ctx, repo, candidates)
}

/// The specialist-generic core of the D7 selection decision. Production always dispatches to the
/// Engineering specialist (via [`select_and_dispatch_with_ctx`]); tests inject a synthetic
/// specialist to drive a controllable no-delta outcome without a live pi. The specialist parameter
/// changes NOTHING about the guards: quarantine, calibration, and the fail-closed `gate` (run by
/// [`dispatch`]) apply identically to whatever specialist is passed.
pub(crate) fn select_and_dispatch_core<S: Specialist>(
    specialist: &S,
    ctx: &mut Ctx,
    repo: &Value,
    candidates: &[Candidate],
) -> Value {
    let lane = crate::control::paths::repo_name(repo);

    // (1) QUARANTINE: pick the FIRST candidate whose selection key is not quarantined.
    let picked = candidates.iter().find(|c| {
        let key = progress::selection_key(ctx, &c.detail);
        !progress::quarantined(ctx, &key)
    });
    let Some(cand) = picked else {
        // Every candidate is quarantined (or the list was empty) — dispatch nothing, spend nothing.
        // HONEST degraded mode: the lane idles until a quarantine expires (24h) or fresh work
        // arrives. This mirrors progress::all_quarantined_bail's contract (no fabricated activity).
        if !candidates.is_empty() {
            ctx.log(
                "orchestrator: all candidate tasks are QUARANTINED (no state delta in 3 attempts \
                 each) — dispatching nothing this wake (no pi spend); idling until a quarantine \
                 expires (24h) or new work arrives",
            );
        }
        return json!({
            "ok": false,
            "reason": "all_quarantined",
            "lane": lane,
            "dispatched": false,
        });
    };

    let kind = diagnosis_to_task_kind(&cand.diagnosis);
    let key = progress::selection_key(ctx, &cand.detail);
    progress::note_selected(ctx, &key, &cand.detail);
    let pre_hash = progress::state_hash(ctx);

    // (2) CALIBRATION (Code tasks only — a Remediate is a fixed closed-registry action, not a
    // sized coding item). Stamp the pending attempt, then MANDATE decomposition when the assigned
    // model's ship-rate for this size_class is proven low. The directive is APPENDED to the task
    // detail (it never removes a gate) so the specialist runs the smallest slice, not the whole item.
    let mut detail = cand.detail.clone();
    if kind == TaskKind::Code {
        calibration::note_selection(ctx, &cand.size_class);
        if let Some(directive) = calibration::decompose_directive(ctx, &cand.size_class) {
            ctx.log(&format!(
                "orchestrator/calibration: model '{}' ship-rate for '{}'-class items is below the \
                 floor — decompose directive appended (catalog #7)",
                ctx.pi_model, cand.size_class
            ));
            detail.push_str(&format!("\n\n{directive}"));
        }
    }

    // (3) DISPATCH through the existing gated path — money_guard -> pecrt::safety run FIRST,
    // UNCHANGED. A DENY returns the refusal verbatim; record NO strike (no work happened, so it is
    // not a zero-delta COMPLETION). The calibration pending marker stamped above is harmlessly
    // overwritten by the next selection on a denied task (calibration.rs's documented crash/abort
    // contract), so a denied attempt is simply not counted.
    let task = Task::new(kind, &lane, &detail);
    let outcome = dispatch(specialist, &task, repo);
    let denied = outcome.get("ok").and_then(Value::as_bool) == Some(false)
        && (outcome.get("money_guard") == Some(&json!(true))
            || outcome.get("pecrt_safety") == Some(&json!(true)));
    if denied {
        return outcome;
    }

    // Terminal: record the outcome so the quarantine strike counter advances on a real no-delta
    // completion (and the calibration pending resolves via the same hook — wiring point B). NEVER
    // "shipped": the orchestrator lands no ship, so state_hash alone decides the delta.
    let ran_ok = outcome.get("ok").and_then(Value::as_bool) == Some(true);
    let outcome_word = if ran_ok { "noop" } else { "error" };
    progress::record_outcome(ctx, &key, &pre_hash, outcome_word);
    outcome
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
    // D7 test infrastructure: an ISOLATED lane Ctx (temp runtime — never the
    // live runtime/) and a synthetic no-delta Specialist, so the quarantine +
    // calibration wiring can be driven without a live pi or shared state.
    // ===================================================================== #

    /// Per-test unique suffix so parallel tests never collide on a tmp dir (progress.rs pattern).
    fn uniq() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("{:x}_{:x}", nanos, N.fetch_add(1, Ordering::Relaxed))
    }

    /// An isolated lane Ctx over injected tmp paths (mirrors progress.rs::test_ctx): its own
    /// runtime dir + a NON-git repo dir so `ctx.git` yields an empty base sha and every state-hash
    /// component is test-controlled and STABLE (so a synthetic no-delta task truly reads no delta).
    /// runtime = <base>/runtime/<name> so the fleet calibration ledger lands in <base>/runtime.
    fn iso_ctx(name: &str) -> Ctx {
        let base = std::env::temp_dir().join(format!("solomon_orch_d7_{}", uniq()));
        let control = base.join("control");
        let repo = base.join("repo");
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::create_dir_all(&repo);
        let mut c = Ctx::configure(&repo.to_string_lossy(), name, "ollama-cloud", None);
        c.control = control;
        c.runtime = base.join("runtime").join(name);
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        c.backlog = base.join("improver").join(name).join("backlog.md");
        c.lessons = base.join("improver").join(name).join("LESSONS.md");
        let _ = std::fs::create_dir_all(c.backlog.parent().unwrap());
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    /// A synthetic specialist whose `run` reports a controllable exit code and touches NO state —
    /// so a completed dispatch reads as ZERO observable delta (the exact retry-theater shape the
    /// quarantine ledger exists to catch). Its `gate` mirrors the Engineering gate (money_guard ->
    /// pecrt::safety) so the guard order is exercised identically; only `run` is synthetic.
    struct NoDeltaSpecialist {
        exit_code: i64,
    }
    impl Specialist for NoDeltaSpecialist {
        fn name(&self) -> &'static str {
            "no_delta_test"
        }
        fn allowed_tools(&self) -> &'static [&'static str] {
            ENGINEERING_ALLOWED_TOOLS
        }
        fn scope_globs(&self, _repo: &Value) -> Vec<String> {
            Vec::new()
        }
        fn gate(&self, task: &Task, repo: &Value) -> Option<Value> {
            EngineeringSpecialist::new().gate(task, repo)
        }
        fn run(&self, task: &Task, _repo: &Value) -> Value {
            // Touch nothing on disk — a genuine no-observable-delta completion.
            json!({
                "ok": self.exit_code == 0,
                "specialist": "no_delta_test",
                "kind": task.kind.as_str(),
                "lane": task.lane,
                "exit_code": self.exit_code,
            })
        }
    }

    // ===================================================================== #
    // D7 ACCEPTANCE (a): a specialist task dispatched 3x with no state delta is
    // QUARANTINED and the orchestrator selects a DIFFERENT task on the 4th wake.
    // ===================================================================== #
    #[test]
    fn no_delta_task_is_quarantined_and_orchestrator_picks_a_different_task_on_the_4th_wake() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let repo = plain_repo();
        let mut ctx = iso_ctx("sover");
        let spec = NoDeltaSpecialist { exit_code: 0 }; // "ran ok" but moved no state -> no-delta noop

        // The lane's candidate work for this wake: the SAME oversized code item first, a distinct
        // fallback second. The first is the retry-theater task; the second is the different work the
        // orchestrator must fall to once the first is quarantined.
        let stuck = Candidate::new(
            "gate_red_streak",
            "fix the persistently red widget gate",
            "chore",
        );
        let other = Candidate::new(
            "noop_streak",
            "add a regression test for the parser",
            "chore",
        );
        let candidates = vec![stuck.clone(), other.clone()];

        let stuck_key = progress::selection_key(&ctx, &stuck.detail);

        // Wakes 1..=3: the stuck task is picked each time (not yet quarantined) and completes with
        // NO observable state delta -> a strike each time. On the 3rd it crosses QUARANTINE_STRIKES.
        for wake in 1..=3 {
            let out = select_and_dispatch_core(&spec, &mut ctx, &repo, &candidates);
            assert_eq!(out["kind"], "code", "wake {wake}: a code task ran");
            assert_eq!(
                out["lane"], "sover",
                "wake {wake}: the stuck task on the sover lane was picked"
            );
        }
        assert!(
            progress::quarantined(&ctx, &stuck_key),
            "after 3 zero-delta completions the stuck task's key must be quarantined"
        );

        // Wake 4: the stuck task is quarantined, so the orchestrator MUST pick the DIFFERENT
        // candidate. We prove it by the different task's selection key being the one seeded/advanced.
        let other_key = progress::selection_key(&ctx, &other.detail);
        let out4 = select_and_dispatch_core(&spec, &mut ctx, &repo, &candidates);
        assert_eq!(out4["kind"], "code", "wake 4 still dispatches a code task");
        // The orchestrator ran the OTHER task: its ledger entry now exists (note_selected seeded it)
        // and it accrued its first strike (recorded a no-delta completion), while the stuck task's
        // strike count is frozen at the quarantine threshold (it was NOT re-run).
        let other_strikes = read_strikes(&ctx, &other_key);
        assert_eq!(
            other_strikes, 1,
            "the 4th wake ran the DIFFERENT task (1 strike), not the quarantined one"
        );
        let stuck_strikes = read_strikes(&ctx, &stuck_key);
        assert_eq!(
            stuck_strikes,
            progress::QUARANTINE_STRIKES,
            "the quarantined task was NOT re-run on the 4th wake (its strike count is frozen)"
        );

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    #[test]
    fn all_candidates_quarantined_idles_the_wake_out_with_no_dispatch() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let repo = plain_repo();
        let mut ctx = iso_ctx("sover");
        let spec = NoDeltaSpecialist { exit_code: 0 };
        let a = Candidate::new("gate_red_streak", "alpha stuck item", "chore");
        let b = Candidate::new("noop_streak", "beta stuck item", "chore");

        // Force BOTH candidates' keys into quarantine directly.
        for c in [&a, &b] {
            let key = progress::selection_key(&ctx, &c.detail);
            progress::note_selected(&ctx, &key, &c.detail);
            let pre = progress::state_hash(&ctx);
            for _ in 0..progress::QUARANTINE_STRIKES {
                progress::record_outcome(&mut ctx, &key, &pre, "noop");
            }
            assert!(progress::quarantined(&ctx, &key));
        }

        let out = select_and_dispatch_core(&spec, &mut ctx, &repo, &[a, b]);
        assert_eq!(out["ok"], false);
        assert_eq!(out["reason"], "all_quarantined");
        assert_eq!(
            out["dispatched"], false,
            "no task was dispatched (no pi spend)"
        );

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    /// Read a key's `count_no_delta` from the isolated ctx's progress ledger (test helper).
    fn read_strikes(ctx: &Ctx, key: &str) -> i64 {
        let led: Value = std::fs::read_to_string(ctx.runtime.join("progress.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(json!({}));
        led["keys"][key]["count_no_delta"].as_i64().unwrap_or(-1)
    }

    // ===================================================================== #
    // D7 ACCEPTANCE (c): calibration gates task SIZE by the assigned model's
    // measured ship-rate — a size above the model's floor is decomposed.
    // ===================================================================== #
    #[test]
    fn calibration_decomposes_a_task_size_the_model_ships_below_the_floor() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let repo = plain_repo();
        let mut ctx = iso_ctx("sover");
        // Prove THIS ctx's model incapable at "architecture": >= MIN_ATTEMPTS attempts, 0 ships.
        let fleet_dir = ctx.runtime.parent().unwrap().to_path_buf();
        for _ in 0..calibration::MIN_ATTEMPTS {
            calibration::record_outcome_at(&fleet_dir, &ctx.pi_model, "architecture", false);
        }

        // A capturing specialist records the task detail it is asked to run, so we can assert the
        // decompose directive was appended to the SIZED item before dispatch.
        struct CapturingSpecialist {
            seen: std::cell::RefCell<String>,
        }
        impl Specialist for CapturingSpecialist {
            fn name(&self) -> &'static str {
                "capture"
            }
            fn allowed_tools(&self) -> &'static [&'static str] {
                ENGINEERING_ALLOWED_TOOLS
            }
            fn scope_globs(&self, _r: &Value) -> Vec<String> {
                Vec::new()
            }
            fn gate(&self, task: &Task, repo: &Value) -> Option<Value> {
                EngineeringSpecialist::new().gate(task, repo)
            }
            fn run(&self, task: &Task, _r: &Value) -> Value {
                *self.seen.borrow_mut() = task.detail.clone();
                json!({"ok": true, "kind": task.kind.as_str(), "lane": task.lane})
            }
        }
        let cap = CapturingSpecialist {
            seen: std::cell::RefCell::new(String::new()),
        };

        // An ARCHITECTURE-class candidate (proven low): the directive MUST be appended.
        let big = Candidate::new(
            "gate_red_streak",
            "re-architect the ingestion pipeline",
            "architecture",
        );
        select_and_dispatch_core(&cap, &mut ctx, &repo, &[big]);
        let seen_big = cap.seen.borrow().clone();
        assert!(
            seen_big.contains("Task-size calibration (mandatory)"),
            "an above-floor size class must have the decompose directive appended: {seen_big}"
        );
        assert!(
            seen_big.contains("DECOMPOSE"),
            "the appended directive must mandate decomposition: {seen_big}"
        );

        // A CHORE-class candidate (cold cell — no evidence): NO directive, runs whole.
        let small = Candidate::new("noop_streak", "fix a typo in the log line", "chore");
        select_and_dispatch_core(&cap, &mut ctx, &repo, &[small]);
        let seen_small = cap.seen.borrow().clone();
        assert!(
            !seen_small.contains("Task-size calibration"),
            "a class with no low-ship-rate evidence must run WHOLE (no directive): {seen_small}"
        );

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
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
        let task = Task::new(
            TaskKind::Code,
            "sover",
            "add a test for the main entrypoint",
        );

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
        assert!(
            out.get("exit_code").is_some(),
            "an honest exit_code is reported"
        );

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
            assert!(
                !k.as_str().is_empty(),
                "diagnosis '{diag}' produced an empty task kind"
            );
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
            "withdraw",
            "transfer",
            "deposit",
            "buy_ads",
            "pay_invoice",
            "stripe_checkout",
            "ad_spend",
            "send_money",
            "spend_treasury",
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

    // ===================================================================== #
    // D10 ACCEPTANCE: the LIVE improver's implement step now routes through
    // `dispatch_engineering_on_ctx` — the SAME gate-first seam the onboarding
    // path uses. A backlog Code task is DISPATCHED to the Engineering
    // specialist (admitted -> pi call reached); a money/mutation-tagged kind is
    // REFUSED at the gate BEFORE any pi spawn.
    // ===================================================================== #

    #[test]
    fn d10_backlog_code_task_is_dispatched_to_engineering_specialist_through_the_live_seam() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        // An isolated lane ctx (never the live runtime) on an ordinary engineering lane. A plain
        // backlog Code task on this lane is neither money-capable nor a governance target, so the
        // fail-closed gate ADMITS it and the dispatch reaches the Engineering specialist's pi call.
        // No live pi exists in the test env, so run_pi returns an honest failed RunOut (rc != 0,
        // empty stdout) — but never panics — which PROVES admission flowed through to the specialist.
        let mut ctx = iso_ctx("sover");
        let repo = plain_repo();
        let out = dispatch_engineering_on_ctx(
            &mut ctx,
            &repo,
            TaskKind::Code,
            "implement the top backlog item as one small, tested change",
            crate::improver::pi::TIMEOUT_IMPLEMENT,
            None,
        );
        // Admitted -> Ok(RunOut): the coding session was dispatched to the Engineering specialist
        // (the gate returned None; run_pi_on_ctx was reached). We assert the seam ADMITTED and
        // returned a RunOut, not that pi succeeded (there is no pi binary in the test env).
        assert!(
            out.is_ok(),
            "a plain backlog Code task on an ordinary lane must be ADMITTED at the gate and \
             dispatched to the Engineering specialist (got a gate DENY): {out:?}"
        );

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    #[test]
    fn d10_money_tagged_task_is_refused_at_the_gate_before_any_pi_spawn() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        // A money-capable task kind attributed to the same live dispatch seam MUST be DENIED at the
        // gate BEFORE any pi spawn — even on a whitelisted live-money lane (money-OUT is the HARD
        // invariant, no lane exempts it). The Err(refusal) proves no pi was spawned (the Ok arm — the
        // only arm that reaches run_pi_on_ctx — was never taken).
        let mut ctx = iso_ctx("kairos");
        for repo in [plain_repo(), kairos_repo()] {
            for money_kind in [
                "withdraw",
                "transfer",
                "pay_invoice",
                "buy_ads",
                "send_money",
            ] {
                let verdict = dispatch_engineering_on_ctx(
                    &mut ctx,
                    &repo,
                    TaskKind::Remediate(money_kind_static(money_kind)),
                    "attempt a money-out under the guise of a coding task",
                    crate::improver::pi::TIMEOUT_IMPLEMENT,
                    None,
                );
                let refusal = verdict.expect_err(&format!(
                    "money-capable kind '{money_kind}' on repo {repo} must be REFUSED at the gate \
                     before any pi spawn"
                ));
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

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    #[test]
    fn d10_self_governance_mutation_lane_is_refused_at_the_gate_before_any_pi_spawn() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        // A dispatch whose lane names a self-governance surface (whitelist / cycle_budget / skeptic /
        // blast-radius / kill / freshness) MUST be DENIED at the gate before any pi spawn — pecrt
        // safety has NO allow arm for these targets. We drive the live seam with such a lane (ctx.name
        // IS the lane) and assert Err(refusal) with pecrt_safety:true.
        for gov_lane in [
            "whitelist",
            "cycle_budget",
            "skeptic_bypass",
            "blast_radius",
            "kill_gate",
            "freshness_gate",
        ] {
            let mut ctx = iso_ctx(gov_lane);
            let verdict = dispatch_engineering_on_ctx(
                &mut ctx,
                &plain_repo(),
                // Even a plain Code task cannot mutate a governance surface — the gate denies on the
                // TARGET (lane), not the kind.
                TaskKind::Code,
                "attempt to widen a self-governance surface",
                crate::improver::pi::TIMEOUT_IMPLEMENT,
                None,
            );
            let refusal = verdict.expect_err(&format!(
                "a dispatch targeting governance surface '{gov_lane}' must be REFUSED at the gate \
                 before any pi spawn"
            ));
            assert_eq!(refusal["ok"], false);
            assert_eq!(
                refusal["pecrt_safety"], true,
                "the DENY must come from the fail-closed pecrt safety gate (no allow arm): {refusal}"
            );
        }

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ===================================================================== #
    // D10 BYTE-IDENTITY GUARD: every REAL production lane must ADMIT a plain
    // Code task at the gate — otherwise the new dispatch seam would spuriously
    // fire the fail-closed DENY branch and silently kill a legitimate
    // iteration. The gate matches governance targets by case-insensitive
    // SUBSTRING, so this pins that no shipping lane name collides with one
    // (a future lane rename into e.g. "*-tier" / "*kill*" is caught HERE).
    // ===================================================================== #
    #[test]
    fn d10_every_production_lane_admits_a_plain_code_task_at_the_gate() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let eng = EngineeringSpecialist::new();
        // The shipping lanes (repos.json). A rename that introduces a governance substring would make
        // this fail — the intended tripwire.
        for lane in [
            "maki", "sover", "asmodeus", "daedulus", "dotz", "solomon", "kairos",
        ] {
            // A plain Code task on the lane — the exact envelope the live seam builds each iteration.
            let task = Task::new(TaskKind::Code, lane, "implement the top backlog item");
            // The gate must ADMIT (None) so the dispatch proceeds to pi UNCHANGED. Probe against BOTH
            // a legacy row and a live-money row to prove no lane/row combination is spuriously denied.
            for repo in [plain_repo(), kairos_repo()] {
                assert!(
                    eng.gate(&task, &repo).is_none(),
                    "production lane '{lane}' must ADMIT a plain Code task at the gate (byte-identical \
                     to the pre-D10 inline pi call) — it was DENIED, which would silently kill a real \
                     iteration on repo {repo}"
                );
            }
        }

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }
}
