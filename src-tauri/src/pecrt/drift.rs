//! DRIFT GATE for the pecrt dual implementation (audit finding #09, 2026-07-12).
//!
//! `src-tauri/src/pecrt/` (Rust, the shipped exe) and `pecrt.py` (repo root, the doctrine-mandated
//! decision-identical Python mirror) previously had NO mechanical gate keeping them in lockstep —
//! each side pinned its OWN copy of the shared constants, so a one-sided edit drifted silently.
//!
//! The gate is a single committed golden file, `pecrt_golden.json` (repo root, next to pecrt.py),
//! that BOTH implementations must match:
//!   * these Rust tests assert every shared decision constant equals the golden byte-for-byte
//!     (runs on every `cargo test`, no Python required), and
//!   * `pecrt.py`'s self-check asserts the SAME equality on the Python side (runs whenever the
//!     mirror runs; the test below also invokes it via `python`/`py` when a launcher exists, so a
//!     machine with Python gets the full cross-impl check inside `cargo test`).
//!
//! A change to either side without updating the golden (and therefore the other side) breaks a gate.

#[cfg(test)]
mod tests {
    use crate::pecrt::{bus, safety, warm};
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// repo root = the parent of src-tauri (CARGO_MANIFEST_DIR).
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("src-tauri has a parent (the repo root)")
            .to_path_buf()
    }

    fn golden() -> Value {
        let p = repo_root().join("pecrt_golden.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| {
            panic!(
                "pecrt_golden.json missing/unreadable at {} ({e}) — the dual-implementation drift \
                 gate requires the committed golden file",
                p.display()
            )
        });
        serde_json::from_str(&raw).expect("pecrt_golden.json parses as JSON")
    }

    #[test]
    fn golden_pins_stable_prefix_and_caps() {
        let g = golden();
        assert_eq!(
            g["stable_prefix"]
                .as_str()
                .expect("golden stable_prefix is a string"),
            warm::STABLE_PREFIX,
            "STABLE_PREFIX drifted from pecrt_golden.json — update BOTH pecrt.py and the golden \
             together (the prefix is a provider-cache contract; silent drift kills cache hits AND \
             the decision mirror)"
        );
        assert_eq!(
            g["working_max_entries"].as_u64(),
            Some(warm::WORKING_MAX_ENTRIES as u64)
        );
        assert_eq!(
            g["working_max_bytes"].as_u64(),
            Some(warm::WORKING_MAX_BYTES as u64)
        );
    }

    #[test]
    fn golden_pins_summary_markers_exactly() {
        let g = golden();
        let markers: Vec<&str> = g["summary_markers"]
            .as_array()
            .expect("golden summary_markers is an array")
            .iter()
            .map(|v| v.as_str().expect("marker is a string"))
            .collect();
        assert_eq!(
            markers,
            warm::SUMMARY_MARKERS,
            "SUMMARY_MARKERS drifted (order + content are both contract) — update pecrt.py's \
             _SUMMARY_MARKERS and pecrt_golden.json in the same change"
        );
    }

    #[test]
    fn golden_pins_forbidden_targets_exactly() {
        let g = golden();
        let targets: Vec<&str> = g["forbidden_targets"]
            .as_array()
            .expect("golden forbidden_targets is an array")
            .iter()
            .map(|v| v.as_str().expect("target is a string"))
            .collect();
        assert_eq!(
            targets,
            safety::FORBIDDEN_TARGETS,
            "FORBIDDEN_TARGETS drifted — the safety closure list must stay identical in \
             safety.rs, pecrt.py, and pecrt_golden.json"
        );
    }

    #[test]
    fn golden_pins_wake_priorities() {
        let g = golden();
        let all = [
            bus::WakeSource::Kill,
            bus::WakeSource::FreshData,
            bus::WakeSource::OpsChange,
            bus::WakeSource::ProviderRecovery,
            bus::WakeSource::FloorElapsed,
            bus::WakeSource::FileAppend,
            bus::WakeSource::SqliteRow,
        ];
        let map = g["wake_priorities"]
            .as_object()
            .expect("golden wake_priorities is an object");
        assert_eq!(map.len(), all.len(), "wake source count drifted: {map:?}");
        for s in all {
            assert_eq!(
                map.get(s.tag()).and_then(Value::as_u64),
                Some(s.priority() as u64),
                "priority for '{}' drifted between bus.rs and pecrt_golden.json",
                s.tag()
            );
        }
    }

    /// The LIVE cross-impl gate: run `python pecrt.py` (its self-check now also verifies the golden
    /// on the Python side). A FAILING self-check fails this test — that IS the drift gate firing.
    /// A machine with no Python launcher degrades gracefully to the golden-equality tests above
    /// (both sides still pin the same committed golden, so one-sided drift is still caught the
    /// next time either gate runs).
    #[test]
    fn python_mirror_exists_and_self_check_passes_when_python_available() {
        let py = repo_root().join("pecrt.py");
        assert!(
            py.exists(),
            "pecrt.py missing at {} — the decision mirror is doctrine-mandated; if it was \
             deliberately retired, delete this gate + pecrt_golden.json in the same change",
            py.display()
        );
        let root = repo_root();
        let script = py.to_string_lossy().to_string();
        for launcher in ["python", "py"] {
            match crate::control::proc::run(
                &[launcher, script.as_str()],
                Some(root.as_path()),
                Some(Duration::from_secs(120)),
            ) {
                Ok(out) => {
                    assert!(
                        out.ok(),
                        "`{launcher} pecrt.py` self-check FAILED (exit {}) — the Python mirror \
                         drifted from the Rust implementation/golden:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                        out.code,
                        out.stdout,
                        out.stderr
                    );
                    return; // ran + passed on this launcher — the live gate is green
                }
                Err(_) => continue, // launcher not on this machine — try the next
            }
        }
        eprintln!(
            "pecrt drift gate: no python/py launcher found — skipped the live `python pecrt.py` \
             self-check (golden-equality tests still enforce the shared constants)"
        );
    }
}
