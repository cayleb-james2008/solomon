//! Native Rust port of improver/run_improver.py — Solomon's per-repo RSI loop runner (Phase 2).
//!
//! Behavior is bug-for-bug with run_improver.py. The Python module uses module-level GLOBALS; in
//! Rust those become ONE [`ctx::Ctx`] struct threaded as `&mut Ctx` / `&Ctx` through every function.
//! This phase builds that `Ctx` + its setup/IO methods (configure, registry refresh, phase config,
//! env/redaction, logging, heartbeat, history, git/gh). The pi/gates/escalation/ship/iteration
//! modules come later and call the `Ctx` API built here.
//!
//! The port-ready reference spec (adversarially verified, with golden vectors) lives at
//! `src-tauri/run-improver-port-spec.json`; the `cli_and_loop` + `pi_agent_contract` areas back this
//! module. Bridge-style dict returns are `serde_json::Value` with byte-identical keys to the Python
//! dicts. `serde_json`'s `preserve_order` feature is enabled crate-wide, so `to_string_pretty`
//! preserves heartbeat key-insertion order (the dashboard reads `heartbeat.json` whole).
#![allow(dead_code)]

pub mod backlog;
pub mod brain;
pub mod budget;
pub mod build_sem;
pub mod calibration;
pub mod ctx;
pub mod escalation;
pub mod freshness;
pub mod gates;
pub mod gitops;
pub mod iteration;
pub mod oneshot;
pub mod outcome_critique;
pub mod park;
pub mod phases;
pub mod pi;
pub mod progress;
pub mod run;
pub mod ship;
pub mod tiers;
pub mod visual;
