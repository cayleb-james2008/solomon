//! Ops plane (Solomon v2, Phase 1) — ground-truth probes + honest fleet status.
//!
//! The code plane (control/improver/supervisor/watchdog) answers "is the LOOP alive?"; this module
//! answers "is the PRODUCT doing its job?" — deterministic Rust only, no LLM anywhere. Every
//! verdict is a file stat, a JSON field, a regex count over a log tail, a read-only SQLite SELECT,
//! a process-table match, or a localhost HTTP status line. This is the mechanical fix for the old
//! fleet-supervisor failure mode: "all healthy" while Sover wasn't posting and Asmodeus wasn't
//! trading.
//!
//!   - `registry` — ops.json (sibling of repos.json, joined to it BY NAME), lenient Value parse
//!   - `probe` — the per-kind evaluators (file_age, json_field, jsonl_tail, log_grep, process,
//!     sqlite_query, cmd, http_get, git_sha_match, file_exists)
//!   - `outcomes` — the sweep: run all probes, persist verdicts + fleet status + incident
//!     transitions under runtime/, and the `solomon probe` CLI entry
//!
//! All state is file-based under runtime/ (gitignored): runtime/<name>/probes_verdict.json,
//! runtime/ops_status.json, append-only runtime/_incidents.jsonl. No scheduled task exists and
//! none may be created (operator rule) — the sweep runs only inside the visibly-open Solomon.exe
//! (watchdog graft) or the `solomon probe` CLI; the first sweep after process start reports the
//! blind window since the previous ops_status.json.
#![allow(dead_code)]

pub mod fleet_ledger;
pub mod ledger;
pub mod outcomes;
pub mod probe;
pub mod registry;

/// A probe verdict color. Ordering is severity (green < yellow < red) so `max` = worst-wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Green = 0,
    Yellow = 1,
    Red = 2,
}

impl Status {
    /// The lowercase wire string persisted in probes_verdict.json / ops_status.json.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Green => "green",
            Status::Yellow => "yellow",
            Status::Red => "red",
        }
    }

    /// Parse the wire string back (unknown -> None, callers treat as "no prior state").
    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "green" => Some(Status::Green),
            "yellow" => Some(Status::Yellow),
            "red" => Some(Status::Red),
            _ => None,
        }
    }

    /// Worst-wins rollup: max severity of the pair.
    pub fn worst(a: Status, b: Status) -> Status {
        std::cmp::max(a, b)
    }
}

/// `solomon probe` exit-code contract: 0 = all green, 3 = worst is yellow, 4 = any red.
/// (2 stays the usage-error code, matching run_headless / improver::run::parse_args.)
pub fn exit_code(worst: Status) -> i32 {
    match worst {
        Status::Green => 0,
        Status::Yellow => 3,
        Status::Red => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- worst-wins ordering --------
    #[test]
    fn status_worst_is_max_severity() {
        assert_eq!(Status::worst(Status::Green, Status::Green), Status::Green);
        assert_eq!(Status::worst(Status::Green, Status::Yellow), Status::Yellow);
        assert_eq!(Status::worst(Status::Yellow, Status::Red), Status::Red);
        assert_eq!(Status::worst(Status::Red, Status::Green), Status::Red);
    }

    // -------- wire round-trip --------
    #[test]
    fn status_wire_round_trips() {
        for s in [Status::Green, Status::Yellow, Status::Red] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        assert_eq!(Status::parse("purple"), None);
        assert_eq!(Status::parse(""), None);
    }

    // -------- exit-code mapping (the CLI contract) --------
    #[test]
    fn exit_code_mapping() {
        assert_eq!(exit_code(Status::Green), 0);
        assert_eq!(exit_code(Status::Yellow), 3);
        assert_eq!(exit_code(Status::Red), 4);
    }
}
