//! The fleet-wide REVENUE ledger (Phase 0.1) — the "Money Truth" file.
//!
//! ============================ WHY THIS EXISTS ============================
//! The June post-mortem showed nobody could answer "did the fleet make any money
//! today?" — Sover published posts but nobody recorded PDF sales, Asmodeus moved
//! equity but revenue was never booked, and the outcomes ledger (ops::ledger)
//! tracks BUSINESS OUTCOMES (posts/equity/trades) per 24 h window but NOT revenue
//! dollars. This module is the missing revenue ledger: one append-only JSONL
//! line per revenue event, fleet-wide — the single source of truth for "$0 or $1".
//!
//! COMPLEMENTARY to money_guard.rs: the money_guard prevents money LEAVING
//! (NO-MONEY-OUT, fail-closed); this ledger records money ARRIVING. Together
//! they form the money boundary: nothing leaves without a human, every dollar
//! that arrives is recorded.
//!
//! COMPLEMENTARY to ops::ledger (the OUTCOMES ledger): outcomes = "what did the
//! product DO?" (posts published, equity moved, trades filled); fleet_ledger =
//! "what did the product EARN?" (revenue dollars, by source). Different
//! questions, different ledgers, same append-only JSONL discipline.
//!
//! FAIL-CLOSED at boot: if fleet_ledger.jsonl is missing we auto-provision it
//! with the schema header; if the path is UNWRITABLE Solomon refuses to start
//! (the same fail-closed doctrine as money_guard — a Solomon that cannot record
//! revenue is a Solomon that cannot prove $0 or $1, and must not run).
//! =========================================================================
#![allow(dead_code)]

use crate::control::paths;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The schema-row marker: rows with `project="_schema"` are documentation, not
/// data. Readers skip them; `ensure_exists` writes one as the file's first line
/// so the schema is self-describing (JSONL has no comments).
pub const SCHEMA_PROJECT: &str = "_schema";

/// The active fleet projects whose revenue this ledger records. (The seed file
/// carries a zero-row for each; future revenue events append against these
/// names. A project not in this list is a bug — validation does NOT enforce
/// membership yet, but readers may.)
pub const FLEET_PROJECTS: &[&str] = &[
    "sover",
    "solomon",
    "asmodeus",
    "dotz",
    "maki",
    "pantheon",
    "daedulus",
    "landing-page-resume",
];

/// One revenue line in the fleet ledger. Serialized as one JSON object per line
/// (JSONL). `note` is optional and omitted from the wire when absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// The project this revenue event belongs to (one of FLEET_PROJECTS, or
    /// "_schema" for the documentation header row).
    pub project: String,
    /// ISO 8601 UTC timestamp, e.g. "2026-07-15T12:00:00Z".
    pub ts: String,
    /// Revenue in USD. Zero is valid (a baseline / no-sale row). Negative is
    /// valid (a refund). NaN is not.
    pub revenue_usd: f64,
    /// Cost in USD (the cost of earning that revenue). Zero is valid. Negative
    /// is valid (a cost reversal). NaN is not.
    pub cost_usd: f64,
    /// The revenue source: "stripe", "gumroad", "affiliate", "compute",
    /// "manual", etc.
    pub source: String,
    /// Optional human-readable description. Omitted from the wire when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// HERE/fleet_ledger.jsonl — the fleet-wide revenue ledger at the Solomon root.
/// (Under `cargo test`, `paths::here()` redirects to a temp home, so tests
/// never touch the real operator ledger — see control::paths.)
pub fn path() -> PathBuf {
    paths::here().join("fleet_ledger.jsonl")
}

/// The schema-document header row (project="_schema"). Written as the first
/// line when the ledger is provisioned so the file is self-describing.
fn schema_row() -> LedgerEntry {
    LedgerEntry {
        project: SCHEMA_PROJECT.to_string(),
        ts: "2026-07-15T00:00:00Z".to_string(),
        revenue_usd: 0.0,
        cost_usd: 0.0,
        source: SCHEMA_PROJECT.to_string(),
        note: Some(
            "fleet revenue ledger — schema: {project, ts, revenue_usd, cost_usd, source, note}. \
             project=_schema rows are documentation, not data."
                .to_string(),
        ),
    }
}

/// Validate a ledger entry BEFORE it is written. Rejects empty `project`,
/// `ts`, or `source`, and NaN `revenue_usd`/`cost_usd`. Negative dollar values
/// are ALLOWED (refunds / cost reversals are real events). Pure — no IO.
pub fn validate(entry: &LedgerEntry) -> Result<(), String> {
    if entry.project.trim().is_empty() {
        return Err("fleet_ledger: entry.project must be non-empty".into());
    }
    if entry.ts.trim().is_empty() {
        return Err("fleet_ledger: entry.ts must be a non-empty ISO 8601 UTC timestamp".into());
    }
    if entry.source.trim().is_empty() {
        return Err("fleet_ledger: entry.source must be non-empty".into());
    }
    if entry.revenue_usd.is_nan() {
        return Err("fleet_ledger: entry.revenue_usd must be a number (not NaN)".into());
    }
    if entry.cost_usd.is_nan() {
        return Err("fleet_ledger: entry.cost_usd must be a number (not NaN)".into());
    }
    Ok(())
}

/// Append one revenue line to the production ledger at `path()`. Validates
/// first (rejects empty project/ts/source, NaN dollars), then atomically
/// appends one JSON line with a trailing newline. The append is atomic per
/// line (OpenOptions append mode + one writeln!) — the same pattern
/// ops::ledger::append_daily uses.
pub fn append(entry: &LedgerEntry) -> Result<(), String> {
    append_at(entry, &path())
}

/// Path-parameterized core of [`append`] so tests run against isolated temp
/// files (the production caller always passes [`path`]).
fn append_at(entry: &LedgerEntry, target: &Path) -> Result<(), String> {
    validate(entry)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("fleet_ledger: create_dir_all {}: {e}", target.display()))?;
    }
    let line = serde_json::to_string(entry)
        .map_err(|e| format!("fleet_ledger: serialize entry: {e}"))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(target)
        .map_err(|e| format!("fleet_ledger: open {}: {e}", target.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("fleet_ledger: write {}: {e}", target.display()))?;
    Ok(())
}

/// FAIL-CLOSED boot check: ensure the fleet ledger exists and is writable at
/// the Solomon root. If the file is MISSING, auto-provision it with the schema
/// header row. If the path is UNWRITABLE, return an error — the caller (main)
/// must refuse to start. Idempotent: an existing file is NEVER overwritten
/// (revenue data is sacred); we only verify it is appendable.
pub fn ensure_exists() -> Result<(), String> {
    ensure_exists_at(&path())
}

/// Path-parameterized core of [`ensure_exists`] so tests run against isolated
/// temp paths (the production caller always passes [`path`]).
fn ensure_exists_at(target: &Path) -> Result<(), String> {
    let fail = |e: &str| {
        format!(
            "fleet_ledger.jsonl missing or unwritable — refusing to start. Create it at {}. ({e})",
            target.display()
        )
    };
    if target.exists() {
        // Verify writable by opening in append mode (does NOT truncate). An
        // existing file is never overwritten — revenue data is sacred.
        std::fs::OpenOptions::new()
            .append(true)
            .open(target)
            .map_err(|e| fail(&e.to_string()))?;
        return Ok(());
    }
    // Missing → auto-provision with the schema header row.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| fail(&e.to_string()))?;
    }
    let header = serde_json::to_string(&schema_row()).map_err(|e| fail(&e.to_string()))?;
    std::fs::write(target, format!("{header}\n")).map_err(|e| fail(&e.to_string()))?;
    Ok(())
}

// --------------------------------------------------------------------------- //
// tests
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "solomon_fleet_ledger_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        ))
    }

    fn valid_entry() -> LedgerEntry {
        LedgerEntry {
            project: "sover".into(),
            ts: "2026-07-15T12:00:00Z".into(),
            revenue_usd: 12.50,
            cost_usd: 0.0,
            source: "stripe".into(),
            note: Some("first PDF sale".into()),
        }
    }

    // -------- append writes exactly one valid JSON line --------
    #[test]
    fn test_append_writes_one_line() {
        let p = temp_path("append.jsonl");
        // Start from an empty file (no schema row — append works on any file).
        std::fs::write(&p, "").unwrap();
        append_at(&valid_entry(), &p).expect("append should succeed");
        let content = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "exactly one data line after one append");
        // The line must be valid JSON matching the entry.
        let got: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(got["project"], json!("sover"));
        assert_eq!(got["revenue_usd"], json!(12.50));
        assert_eq!(got["source"], json!("stripe"));
        assert_eq!(got["note"], json!("first PDF sale"));
        // A trailing newline must be present (one record per line discipline).
        assert!(content.ends_with('\n'), "append must end with a newline");
        let _ = std::fs::remove_file(&p);
    }

    // -------- append without a note omits the field cleanly --------
    #[test]
    fn test_append_without_note_omits_field() {
        let p = temp_path("nonote.jsonl");
        let mut e = valid_entry();
        e.note = None;
        append_at(&e, &p).unwrap();
        let got: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&p).unwrap().trim()).unwrap();
        assert!(got.get("note").is_none(), "note must be absent when None");
        let _ = std::fs::remove_file(&p);
    }

    // -------- ensure_exists creates the file with the schema row --------
    #[test]
    fn test_ensure_exists_creates_file() {
        let p = temp_path("ensure_create.jsonl");
        assert!(!p.exists(), "precondition: file must not exist");
        ensure_exists_at(&p).expect("ensure_exists should create the file");
        assert!(p.exists(), "file must exist after ensure_exists");
        let content = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "a freshly provisioned file has exactly the schema row");
        let row: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row["project"], json!("_schema"));
        assert_eq!(row["source"], json!("_schema"));
        let _ = std::fs::remove_file(&p);
    }

    // -------- ensure_exists is idempotent: an existing file is preserved --------
    #[test]
    fn test_ensure_exists_preserves_existing_file() {
        let p = temp_path("ensure_preserve.jsonl");
        // Seed with a real revenue row (not a schema row — real data).
        let seed = serde_json::to_string(&valid_entry()).unwrap();
        std::fs::write(&p, format!("{seed}\n")).unwrap();
        ensure_exists_at(&p).expect("ensure_exists on an existing file should succeed");
        let content = std::fs::read_to_string(&p).unwrap();
        // The revenue row must be UNCHANGED — ensure_exists never overwrites.
        assert_eq!(
            content.trim(),
            seed,
            "ensure_exists must NOT overwrite existing revenue data"
        );
        let _ = std::fs::remove_file(&p);
    }

    // -------- ensure_exists refuses an unwritable path (parent is a file) --------
    #[test]
    fn test_ensure_exists_refuses_unwritable_path() {
        // Make the parent a FILE (not a directory) so create_dir_all / open fails.
        // This reliably fails on every platform — no ACL / permission tricks needed.
        let blocker = temp_path("blocker.tmp");
        std::fs::write(&blocker, "blocker").unwrap();
        let target = blocker.join("fleet_ledger.jsonl"); // parent is a file → unwritable
        let res = ensure_exists_at(&target);
        assert!(res.is_err(), "ensure_exists must fail when the path is unwritable");
        let err = res.unwrap_err();
        assert!(
            err.contains("refusing to start"),
            "error must say 'refusing to start': {err}"
        );
        assert!(
            err.contains("fleet_ledger.jsonl"),
            "error must name the file: {err}"
        );
        let _ = std::fs::remove_file(&blocker);
    }

    // -------- validation rejects empty project / ts / source + NaN dollars --------
    #[test]
    fn test_validation_rejects_empty_project() {
        let mut e = valid_entry();
        e.project = "".into();
        assert!(validate(&e).is_err(), "empty project must be rejected");

        e = valid_entry();
        e.project = "   ".into();
        assert!(validate(&e).is_err(), "whitespace-only project must be rejected");

        e = valid_entry();
        e.ts = "".into();
        assert!(validate(&e).is_err(), "empty ts must be rejected");

        e = valid_entry();
        e.source = "".into();
        assert!(validate(&e).is_err(), "empty source must be rejected");
    }

    #[test]
    fn test_validation_rejects_nan_dollars() {
        let mut e = valid_entry();
        e.revenue_usd = f64::NAN;
        assert!(validate(&e).is_err(), "NaN revenue must be rejected");

        e = valid_entry();
        e.cost_usd = f64::NAN;
        assert!(validate(&e).is_err(), "NaN cost must be rejected");
    }

    // -------- validation ALLOWS negative dollars (refunds / cost reversals) --------
    #[test]
    fn test_validation_allows_negative_dollars() {
        let mut e = valid_entry();
        e.revenue_usd = -5.00; // a refund
        e.cost_usd = -2.00; // a cost reversal
        assert!(validate(&e).is_ok(), "negative dollars (refunds) must be allowed");
    }

    // -------- append_at rejects an invalid entry (validation gates the write) --------
    #[test]
    fn test_append_rejects_invalid_entry() {
        let p = temp_path("reject.jsonl");
        std::fs::write(&p, "").unwrap();
        let mut e = valid_entry();
        e.project = "".into();
        assert!(append_at(&e, &p).is_err(), "append must reject an invalid entry");
        // And the file must be UNCHANGED (no partial write).
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "",
            "a rejected append must not write anything"
        );
        let _ = std::fs::remove_file(&p);
    }

    // -------- the schema row round-trips through serde --------
    #[test]
    fn test_schema_row_round_trips() {
        let row = schema_row();
        let json = serde_json::to_string(&row).unwrap();
        let back: LedgerEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.project, SCHEMA_PROJECT);
        assert_eq!(back.source, SCHEMA_PROJECT);
        assert_eq!(back.revenue_usd, 0.0);
        assert!(back.note.is_some());
    }
}