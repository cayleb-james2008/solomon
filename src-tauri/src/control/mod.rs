//! Native Rust port of control.py — the repos registry, git/gh wrappers, runtime locks, heartbeat
//! reads, branch hygiene, contracts, and the loop runner that backs the `bridge` command.
//!
//! Behavior is bug-for-bug with control.py; functions that back JS bridge calls return
//! `serde_json::Value` whose keys are byte-identical to the Python dicts. The reference spec
//! (with golden vectors + adversarial corrections) lives at `src-tauri/control-port-spec.json`.
//!
//! `proc`/`paths` are the shared foundation; the leaf modules (registry, heartbeat, locks, gh,
//! contracts, branches, keys, runner, apptest_health) build on it.
#![allow(dead_code)]

pub mod paths;
pub mod proc;

pub mod apptest_health;
pub mod branches;
pub mod contracts;
pub mod gh;
pub mod heartbeat;
pub mod keys;
pub mod locks;
pub mod registry;
pub mod runner;
