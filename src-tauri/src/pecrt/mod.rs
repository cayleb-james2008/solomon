//! PECRT Layer 0 — Persistent Event-driven Continuous Reasoning Thread, base layer.
//!
//! This module is a **scheduler + memory WRAPPER**, never a new authority. It answers exactly two
//! questions for a continuous reasoning thread, and it answers them WITHOUT taking any power the
//! existing loop does not already hand it:
//!
//!   1. WHEN should the thread wake?  ([`bus`] — a shared wake-bus that GENERALIZES the existing
//!      per-lane event-driven park in `improver::park` into a source-agnostic bus over file-append,
//!      sqlite-row, and signal-file watchers, standardizing the freshness-probe JSON as the event
//!      schema.)
//!
//!   2. WHAT does it remember cheaply across wakes?  ([`warm`] — a three-tier warm context:
//!      a HARD-BOUNDED working tier rewritten each wake, an append-only DATED observation log of
//!      FACTS, and a read-only ADAPTER over the ledgers Solomon already keeps. `reconstruct_context`
//!      assembles these behind a STABLE prompt prefix so the LLM provider prefix-cache hits.)
//!
//! And it enforces ONE hard safety invariant in code ([`safety`]): the thread is a scheduler, not a
//! governor. Every action it would schedule re-enters the EXISTING gates (money_guard, freshness
//! short-circuit, blast-radius, skeptic). It CANNOT edit the whitelist tiers in repos.json, raise a
//! lane's `cycle_budget`, or bypass the skeptic — those routes are DENIED at classification time,
//! before any IO, exactly the way [`crate::money_guard`] fail-closes money-out.
//!
//! ## Dual implementation
//!
//! Per the continuous-reasoning-thread architecture doctrine (and the established
//! `control.py` <-> `src-tauri/src/control/` port pattern documented in
//! `src-tauri/src/control/contracts.rs` L1-7 and `control-port-spec.json`), this Rust crate has a
//! DECISION-IDENTICAL Python mirror at `pecrt.py` (repo root). The two are bug-for-bug: the same
//! bounds, the same event schema, the same deny/allow verdicts, the same stable-prefix bytes. The
//! Python side exists so the pi-hosted agent and any tooling can reason about the SAME contract the
//! native loop enforces. Where a decision is load-bearing, the Rust doc-comment and the Python
//! docstring state it identically. DRIFT GATE (audit #09): both sides are pinned to the committed
//! golden `pecrt_golden.json` (repo root) — the drift-gate tests (`src/pecrt/drift.rs`) enforce it on every `cargo test`
//! (and run `python pecrt.py` when a launcher exists); pecrt.py's self-check enforces it on the
//! Python side. Change a shared constant ONLY by updating both implementations + the golden together.
//!
//! ## What this module deliberately does NOT do
//!
//!   * It does NOT migrate any ledger. The long-term tier is a read-only adapter; `outcomes.jsonl`,
//!     `runtime/<lane>/freshness.json`, and the progress/calibration ledgers stay exactly where and
//!     how the existing code writes them.
//!   * It does NOT replace `improver::park`. The per-lane loop keeps its own park; `bus` is the
//!     generalized form a future thread-plane driver consumes, sharing the same `WakeSource` taxonomy
//!     and the same freshness OR-rule so the two can never diverge.
//!   * It does NOT hold or dispatch any action. It emits a `ScheduleRequest` verdict; the caller runs
//!     the (unchanged) gate funnel.
#![allow(dead_code)]

pub mod bus;
mod drift; // dual-implementation drift gate: Rust <-> pecrt.py via pecrt_golden.json (audit #09)
pub mod safety;
pub mod warm;

// Crate public surface — the API a future Layer-1 thread-plane driver consumes. Layer 0 is
// foundation, so nothing INSIDE the binary calls these yet; the re-exports are the deliberate
// contract, exercised by the module's own #[test] suites and the pecrt.py decision mirror.
#[allow(unused_imports)]
pub use bus::{FreshnessEvent, WakeReason, WakeSource, WatchSource, next_wake};
#[allow(unused_imports)]
pub use safety::{ScheduleRequest, ScheduleVerdict, classify_schedule, guard_schedule};
#[allow(unused_imports)]
pub use warm::{
    LONG_TERM_ADAPTER_READONLY, ObservationLog, ReconstructedContext, STABLE_PREFIX,
    WORKING_MAX_BYTES, WORKING_MAX_ENTRIES, WarmContext, WorkingTier,
};
