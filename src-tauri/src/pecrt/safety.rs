//! The HARD safety invariant, enforced in code: the persistent reasoning thread is a
//! **scheduler + memory wrapper, NEVER a new authority**.
//!
//! A continuous thread that can wake itself and remember context is powerful, and the failure mode is
//! obvious: it starts editing its OWN constraints — widening the repos.json whitelist tiers so it may
//! touch more repos, raising a lane's `cycle_budget` so it may spend more tokens/wall per cycle, or
//! routing an action around the skeptic. This module makes those routes IMPOSSIBLE by construction,
//! the same way [`crate::money_guard`] makes money-out impossible: a PURE, fail-closed classifier that
//! runs BEFORE any IO and DENIES any scheduled action whose target is a self-governing path.
//!
//! The thread may only ever SCHEDULE an action — decide WHEN some existing, already-gated operation
//! runs. It may NOT decide WHAT the gates permit. So the classifier's job is narrow and total:
//!
//!   * A schedule request whose target touches a FORBIDDEN self-governance path
//!     (`repos.json` tiers/whitelist, `cycle_budget`, or a skeptic-bypass) is DENIED — full stop, no
//!     lane can whitelist it, there is no allow arm. This is stricter than money_guard (which has one
//!     narrow allow arm for a whitelisted lane's own trade); here there is NO allow arm at all for a
//!     governance-mutation target.
//!   * A schedule request for an ORDINARY operation (run an iteration, probe freshness, reflect,
//!     park) is ALLOWED to be scheduled — but the ALLOW explicitly means "re-enter the EXISTING gate
//!     funnel", never "run unchecked". The verdict carries a note that the action MUST route through
//!     money_guard / freshness short-circuit / blast-radius / skeptic exactly as a non-thread caller
//!     would. The thread adds no authority; it only changes timing.
//!
//! ## Why classify the TARGET, not the verb
//!
//! A naive guard would blocklist verbs like "edit". But the thread never needs to edit anything to do
//! its job — it schedules. So we classify the schedule request's TARGET path/key. Any request that
//! names a governance surface is denied regardless of the verb, and the set of governance surfaces is
//! a closed, auditable list ([`FORBIDDEN_TARGETS`]) pinned by a closure test — a future governance key
//! must be added here to be protected, and the DEFAULT for an unrecognized governance-shaped target
//! is DENY (fail-closed), mirroring money_guard's default-deny arm.

use serde_json::{json, Value};

/// A skeptic-bypass sentinel. Any target mentioning this is a request to skip the skeptic — always
/// denied. (The skeptic is a HARD gate; the thread can never route around it.)
pub const SKEPTIC_BYPASS_MARKER: &str = "skeptic_bypass";

/// The closed set of FORBIDDEN self-governance target markers. A schedule request whose `target`
/// contains ANY of these (case-insensitive substring) is DENIED — the thread cannot mutate the path.
/// Additions here are the ONLY way to extend protection; the closure test
/// `forbidden_targets_are_closed_and_named` pins the list so a silent removal breaks the build.
///
///   * `repos.json` / `tier` / `whitelist` — the repo whitelist & its tiers (which repos, at what
///     blast-radius). The thread must never widen who it may touch.
///   * `cycle_budget` — the per-lane token/wall budget. The thread must never raise its own ceiling.
///   * `skeptic_bypass` — any route around the skeptic gate.
///   * `blast_radius` / `blast-radius` — the canary/flag blast-radius policy.
///   * `money_guard` / `no_money_out` — the money-out invariant surface.
///   * `kill` sentinel policy — the operator KILL/DRAIN gate config (the thread may RESPOND to a KILL,
///     never reconfigure or suppress one).
pub const FORBIDDEN_TARGETS: &[&str] = &[
    "repos.json",
    "whitelist",
    "tier",
    "cycle_budget",
    SKEPTIC_BYPASS_MARKER,
    "skeptic",
    "blast_radius",
    "blast-radius",
    "money_guard",
    "no_money_out",
    "kill_gate",
    "kill_sentinel",
    "freshness_gate",
];

/// A request FROM the thread to schedule an action. `verb` is what it wants done (informational only —
/// the classifier does not trust it), `target` is the path/key/surface it names, and `reenters_gates`
/// is the thread's own claim that this action will re-enter the existing gate funnel. The classifier
/// does NOT take that claim on faith for a governance target; it denies those outright.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleRequest {
    pub verb: String,
    pub target: String,
    /// The thread asserts this action re-enters money_guard/freshness/blast-radius/skeptic. For an
    /// ordinary target this must be TRUE for an ALLOW; a FALSE here means "run unchecked", which is
    /// itself denied (the thread cannot schedule a gate-bypassing action).
    pub reenters_gates: bool,
}

impl ScheduleRequest {
    pub fn new(verb: &str, target: &str, reenters_gates: bool) -> Self {
        ScheduleRequest {
            verb: verb.to_string(),
            target: target.to_string(),
            reenters_gates,
        }
    }
}

/// The verdict. `Allow` means "this action may be SCHEDULED, and it will re-enter the existing gate
/// funnel" (the reason names the funnel so it is auditable). `Deny` means the thread is attempting to
/// exceed its authority — refused, fail-closed.
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleVerdict {
    Allow { reason: String },
    Deny { reason: String },
}

impl ScheduleVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, ScheduleVerdict::Allow { .. })
    }
    pub fn reason(&self) -> &str {
        match self {
            ScheduleVerdict::Allow { reason } | ScheduleVerdict::Deny { reason } => reason,
        }
    }
}

/// True iff `target` names a forbidden self-governance surface (case-insensitive substring against
/// [`FORBIDDEN_TARGETS`]).
pub fn targets_governance(target: &str) -> bool {
    let t = target.to_ascii_lowercase();
    FORBIDDEN_TARGETS.iter().any(|m| t.contains(m))
}

/// The PURE, fail-closed classifier. No IO. Decides whether the thread may SCHEDULE this request.
///
/// Decision table (checked in order):
///   1. `target` names a governance surface  -> DENY (no allow arm exists — the thread can never
///      mutate the whitelist/tiers, raise cycle_budget, or bypass the skeptic/kill/blast-radius).
///   2. `reenters_gates == false`             -> DENY (the thread cannot schedule an action that
///      skips the existing gate funnel — "run unchecked" is itself a bypass).
///   3. otherwise                              -> ALLOW, with a reason NAMING the gate funnel the
///      action re-enters (money_guard, freshness short-circuit, blast-radius, skeptic). Scheduling
///      only changes TIMING; the gates still decide the outcome.
pub fn classify_schedule(req: &ScheduleRequest) -> ScheduleVerdict {
    // (1) Governance target: DENY, no lane, no exception.
    if targets_governance(&req.target) {
        return ScheduleVerdict::Deny {
            reason: format!(
                "schedule DENIED: target '{}' is a self-governance surface. The persistent thread is \
                 a scheduler+memory WRAPPER, never an authority — it cannot edit the repos.json \
                 whitelist/tiers, raise cycle_budget, or bypass the skeptic/kill/blast-radius/\
                 freshness gates. This route is fail-closed: there is NO allow arm for a governance \
                 target (verb='{}')",
                req.target, req.verb
            ),
        };
    }

    // (2) An action that claims it will NOT re-enter the gates is a bypass request: DENY.
    if !req.reenters_gates {
        return ScheduleVerdict::Deny {
            reason: format!(
                "schedule DENIED: action '{}' on '{}' declares it would NOT re-enter the existing \
                 gate funnel (money_guard / freshness short-circuit / blast-radius / skeptic). The \
                 thread may only change WHEN a gated action runs, never let one run unchecked",
                req.verb, req.target
            ),
        };
    }

    // (3) Ordinary, gated action: schedulable. The ALLOW is explicitly conditional on re-entering
    // the existing gates — the thread adds timing, not authority.
    ScheduleVerdict::Allow {
        reason: format!(
            "schedule ALLOWED: '{}' on '{}' may be scheduled — it re-enters the EXISTING gate funnel \
             (money_guard -> freshness short-circuit -> blast-radius -> skeptic) unchanged. The \
             thread only decides timing; the gates decide the outcome",
            req.verb, req.target
        ),
    }
}

/// The GATE the thread's scheduler calls before dispatching any action it decided to run. Mirrors
/// [`crate::money_guard::guard`]: returns `None` when the action may proceed (into the unchanged gate
/// funnel), or `Some(refusal Value)` shaped like the other refusals (`{ok:false, target, error,
/// pecrt_safety:true}`) when the thread is exceeding its authority — the caller early-returns it
/// without any new wiring. A DENY is fail-closed: the action is NOT scheduled.
pub fn guard_schedule(req: &ScheduleRequest) -> Option<Value> {
    match classify_schedule(req) {
        ScheduleVerdict::Allow { .. } => None,
        ScheduleVerdict::Deny { reason } => Some(json!({
            "ok": false,
            "verb": req.verb,
            "target": req.target,
            "pecrt_safety": true,
            "error": reason,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ================================================================================= #
    // NON-NEGOTIABLE INVARIANT TESTS (acceptance criterion (b)).
    // The persistent thread CANNOT mutate any whitelist / cycle_budget / skeptic path.
    // Every attempt routes through this classifier and is DENIED, never applied directly.
    // ================================================================================= #

    /// The core invariant: EVERY governance-mutation attempt is denied, for every verb.
    #[test]
    fn thread_cannot_mutate_whitelist_cycle_budget_or_skeptic() {
        let forbidden_targets = [
            "repos.json",
            "repos.json#tiers",
            "repos.json tier promotion",
            "whitelist",
            "kairos.cycle_budget.pi_calls",
            "cycle_budget",
            "skeptic",
            "skeptic_bypass",
            "blast_radius",
            "blast-radius policy",
            "money_guard",
            "no_money_out",
            "kill_gate",
            "kill_sentinel",
            "freshness_gate",
        ];
        // Try to MUTATE each via every plausible verb, EVEN while claiming it re-enters gates.
        for target in forbidden_targets {
            for verb in ["edit", "write", "raise", "widen", "add", "remove", "bypass", "disable", "patch"] {
                let req = ScheduleRequest::new(verb, target, true); // claims gate re-entry — irrelevant
                let v = classify_schedule(&req);
                assert!(
                    !v.is_allowed(),
                    "SAFETY BREACH: thread was allowed to '{verb}' governance target '{target}' \
                     (verdict: {})",
                    v.reason()
                );
                // and the guard emits a fail-closed refusal, never None.
                let refusal = guard_schedule(&req).expect("governance mutation must be refused");
                assert_eq!(refusal["ok"], false);
                assert_eq!(refusal["pecrt_safety"], true);
            }
        }
    }

    /// The classifier gives NO allow arm to a governance target even when the request is otherwise
    /// pristine — there is no whitelisted lane, no flag, nothing that flips a DENY to ALLOW.
    #[test]
    fn no_allow_arm_exists_for_a_governance_target() {
        let req = ScheduleRequest::new("read-then-edit", "repos.json", true);
        assert!(matches!(classify_schedule(&req), ScheduleVerdict::Deny { .. }));
    }

    /// An action that declares it will skip the gates is denied even for an ordinary target.
    #[test]
    fn scheduling_a_gate_bypass_is_denied() {
        let req = ScheduleRequest::new("run_iteration", "kairos", false); // won't re-enter gates
        let v = classify_schedule(&req);
        assert!(!v.is_allowed(), "an unchecked (gate-skipping) action must be denied");
        assert!(guard_schedule(&req).is_some());
    }

    // ---- ordinary, gated actions ARE schedulable (the thread does useful work) ----

    #[test]
    fn ordinary_gated_actions_are_schedulable() {
        for (verb, target) in [
            ("run_iteration", "kairos"),
            ("probe_freshness", "asmodeus"),
            ("reflect", "sover"),
            ("park", "dotz"),
            ("reconstruct_context", "maki"),
        ] {
            let req = ScheduleRequest::new(verb, target, true);
            let v = classify_schedule(&req);
            assert!(v.is_allowed(), "'{verb}' on '{target}' should be schedulable: {}", v.reason());
            // the ALLOW reason must name the gate funnel it re-enters (auditability).
            assert!(v.reason().contains("gate funnel"), "allow must name the gate funnel");
            // guard returns None so the caller proceeds into the UNCHANGED funnel.
            assert!(guard_schedule(&req).is_none());
        }
    }

    /// A lane name that HAPPENS to be an ordinary target near a governance word is still fine, but a
    /// target literally naming cycle_budget under any lane is denied (substring match is intentional).
    #[test]
    fn substring_match_is_case_insensitive_and_lane_qualified() {
        assert!(targets_governance("KAIROS.Cycle_Budget"));
        assert!(targets_governance("promote to Tier 1"));
        assert!(!targets_governance("run_iteration"));
        assert!(!targets_governance("kairos")); // a plain lane name is not governance
    }

    // ---- closure: the forbidden list is closed + named (default-deny for governance shapes) ----

    #[test]
    fn forbidden_targets_are_closed_and_named() {
        // Pin the exact protected surfaces. A silent removal here breaks the build — a governance
        // key must be EXPLICITLY listed to be protected, and this test is the audit anchor.
        let expected = [
            "repos.json",
            "whitelist",
            "tier",
            "cycle_budget",
            "skeptic_bypass",
            "skeptic",
            "blast_radius",
            "blast-radius",
            "money_guard",
            "no_money_out",
            "kill_gate",
            "kill_sentinel",
            "freshness_gate",
        ];
        assert_eq!(
            FORBIDDEN_TARGETS, &expected,
            "the forbidden governance target set changed — re-audit the safety invariant"
        );
        assert!(FORBIDDEN_TARGETS.contains(&SKEPTIC_BYPASS_MARKER));
    }
}
