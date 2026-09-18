//! The unified `_pending_approvals` operator surface — ONE deterministic, cheap (file-IO-only,
//! no LLM) place the operator reads to see what Solomon is ABOUT TO DO autonomously and what
//! still waits on a human hand.
//!
//! TRUTH NOTE (2026-07-16, commits b6f148a / 4febc11 / 7ea9c23): growth publish, cold-outreach
//! send, and self-tooling validate+invoke are AUTONOMOUS — they run behind AUTOMATED gates only
//! (persona/content checks, per-lane-per-day rate caps, the operator-supplied target list, lint +
//! sandboxed dry-run + sha256) with NO operator `approved: true` wait. This surface therefore
//! lists each lane's IMMINENT autonomous action with the EXACT edit that STOPS it, and names what
//! REMAINS human-gated: money-out (money_guard, fail-closed), live-capital changes,
//! `growth_publish` live-mode promotion, SMTP credential provisioning, and external platform
//! logins. `regenerate` (ridden on the FAST deterministic core of `ceo::tick`, every ~2 min
//! sweep) rewrites `runtime/_pending_approvals.md` (human) + `.json` (dashboard).
//!
//! HARD INVARIANTS (unchanged by this module):
//!   * READ-ONLY over the listed artifacts — this surface never publishes, never sends, never
//!     edits a draft, never touches the manifest. The stop edits listed here are instructions for
//!     a HUMAN hand.
//!   * The growth rows use the SAME `growth::auto_publish_content_check` the auto-publish seam
//!     ships through (re-used, never forked) — the surface and the seam can never drift apart.
//!   * A corrupt log/manifest omits ITS row (catch_unwind per lane; unparseable manifest reads as
//!     empty) — the surface is still written, never wedged, never a truncation (edge E19/E16).
#![allow(dead_code)]

use crate::control::{paths, proc, registry};
use crate::pecrt::warm::ObservationLog;
use chrono::Utc;
use serde_json::{Value, json};
use std::path::PathBuf;

/// `runtime/_pending_approvals.md` — the human-readable surface (regenerated every tick).
fn md_path() -> PathBuf {
    paths::here().join("runtime").join("_pending_approvals.md")
}

/// `runtime/_pending_approvals.json` — the machine-readable twin for the dashboard/GUI.
fn json_path() -> PathBuf {
    paths::here()
        .join("runtime")
        .join("_pending_approvals.json")
}

/// `runtime/_tools/tools_manifest.json` — the self-tooling manifest (see `ceo::self_tooling`).
/// Defined HERE (this surface is its first reader) and re-used by `self_tooling`, so the path can
/// never drift between the writer and this reader. Deliberately under gitignored `runtime/` —
/// NEVER a `provenance::TRACKED` watched file (a runtime artifact, not tracked source).
pub(crate) fn tools_manifest_path() -> PathBuf {
    paths::here()
        .join("runtime")
        .join("_tools")
        .join("tools_manifest.json")
}

// --------------------------------------------------------------------------- //
// collectors — pure over injected rows / the manifest Value (unit-tested)
// --------------------------------------------------------------------------- //

/// Growth drafts queued for AUTONOMOUS publish. `rows` is `[{lane, path, newest}]` (the newest
/// drafts line per public lane, assembled by `regenerate`); a row surfaces iff its newest line is
/// genuine auto-publishable CONTENT per the SAME `growth::auto_publish_content_check` the
/// auto-publish seam ships through (blank/control/persona-violating lines will not publish, so
/// they are not listed as imminent). Pure.
pub(crate) fn collect_growth_pending(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .filter_map(|r| {
            let (lane, path, newest) = row_parts(r)?;
            if crate::ceo::growth::auto_publish_content_check(newest).is_err() {
                return None; // not publishable content — the seam would refuse it, nothing imminent
            }
            Some(json!({
                "lane": lane,
                "path": path,
                "preview": super::cap_line(newest, 200),
                "action": format!(
                    "will auto-publish at next slow-tail sweep (once/lane/day cap; dry-run unless \
                     the lane's growth_publish.mode is \"live\") unless removed — delete the newest \
                     line of {path} to stop it"
                ),
            }))
        })
        .collect()
}

/// Outreach drafts queued for AUTONOMOUS send — the composer wrote each draft's full sendable
/// payload to the lane's `outreach_outbox.jsonl` twin, and the auto-send seam ships the oldest
/// unsent entry behind the operator-target guard + rate caps (SMTP creds are a data dependency the
/// operator provisions). The resolved `to`/`subject` are pulled out of the draft fact so the
/// operator sees WHO will be emailed before it fires. Pure.
pub(crate) fn collect_outreach_pending(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .filter_map(|r| {
            let (lane, path, newest) = row_parts(r)?;
            if crate::ceo::growth::draft_line_approved(newest) {
                return None; // a hand-edited JSON control line is not a queued composer draft
            }
            let outbox = path.replace("outreach_drafts.jsonl", "outreach_outbox.jsonl");
            Some(json!({
                "lane": lane,
                "path": path,
                "to": fact_field(newest, "to="),
                "subject": fact_subject(newest),
                "preview": super::cap_line(newest, 200),
                "action": format!(
                    "auto-sends (no approval wait) once SMTP creds are present and the recipient \
                     is on the operator target list — remove the matching entry from {outbox} to \
                     stop it"
                ),
            }))
        })
        .collect()
}

/// Self-authored tools NOT auto-registered as approved — legacy pre-autonomy manifest entries
/// (`approved` != exactly boolean `true`; the 7ea9c23 pipeline registers straight to
/// `approved: true`). TRUTH: since 7ea9c23 the invoke gates are lint + sandboxed dry-run + sha256
/// — the `approved` flag is NO LONGER consulted, so a dry-run-passed entry here invokes
/// AUTONOMOUSLY, while a dry-run-pending/failed entry is an inert stub the autonomous pipeline
/// will not resurrect. Each row carries the truthful state + the retire edit. Pure over the
/// manifest Value; sorted for determinism.
pub(crate) fn collect_tools_pending(manifest: &Value) -> Vec<Value> {
    let Some(tools) = manifest.get("tools").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = tools
        .iter()
        .filter(|(_, e)| e.get("approved").and_then(Value::as_bool) != Some(true))
        .map(|(name, e)| {
            let pending_validation =
                e.pointer("/dry_run/pending").and_then(Value::as_bool) == Some(true);
            let dry_run_passed = e
                .pointer("/dry_run/passed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let retire = format!(
                "delete tools.{name} from runtime\\_tools\\tools_manifest.json to retire it"
            );
            let action = if dry_run_passed && !pending_validation {
                format!(
                    "invokes AUTONOMOUSLY behind the lint + sandboxed dry-run + sha256 gates (the \
                     legacy \"approved\" flag is no longer consulted) — {retire}"
                )
            } else {
                format!(
                    "inert stub: its sandboxed dry-run never passed under the pre-autonomy flow \
                     and the autonomous pipeline only registers tools it authors — {retire}"
                )
            };
            json!({
                "name": name,
                "purpose": e.get("purpose").and_then(Value::as_str).unwrap_or(""),
                "lint_passed": e.pointer("/lint/passed").and_then(Value::as_bool).unwrap_or(false),
                "dry_run_passed": dry_run_passed,
                "pending_validation": pending_validation,
                "action": action,
            })
        })
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// Destructure one injected row into (lane, path, newest); None when the row is unusable (nameless
/// lane / blank newest line) — an unusable row is omitted, never a panic. Pure.
fn row_parts(r: &Value) -> Option<(&str, &str, &str)> {
    let lane = r.get("lane").and_then(Value::as_str).unwrap_or("");
    let path = r.get("path").and_then(Value::as_str).unwrap_or("");
    let newest = r.get("newest").and_then(Value::as_str).unwrap_or("");
    if lane.is_empty() || newest.trim().is_empty() {
        return None;
    }
    Some((lane, path, newest))
}

/// Extract a space-terminated `<key><value>` token from a draft fact line (pure — e.g.
/// `fact_field(line, "to=")` pulls the recipient out of an outreach draft fact).
pub(crate) fn fact_field(line: &str, key: &str) -> String {
    line.find(key)
        .map(|i| {
            let rest = &line[i + key.len()..];
            rest.split_whitespace().next().unwrap_or("").to_string()
        })
        .unwrap_or_default()
}

/// Extract the `subj=` value, which runs to the ` :: ` body separator (subjects carry spaces, so a
/// plain whitespace split would truncate them). Pure.
pub(crate) fn fact_subject(line: &str) -> String {
    line.find("subj=")
        .map(|i| {
            let rest = &line[i + "subj=".len()..];
            rest.split(" ::").next().unwrap_or("").trim().to_string()
        })
        .unwrap_or_default()
}

// --------------------------------------------------------------------------- //
// rendering (pure — the test pins the shape)
// --------------------------------------------------------------------------- //

/// Render the human-readable surface. Pure so the `#[test]` pins the markdown: a header with the
/// counts + the AUTONOMY truth note (what runs without approval vs what stays human-gated), one
/// section per non-empty category, each item carrying its truthful state + the EXACT stop/retire
/// edit; all-empty renders an honest "Nothing pending." body.
pub(crate) fn render_md(growth: &[Value], outreach: &[Value], tools: &[Value]) -> String {
    let mut md = String::new();
    md.push_str("# Pending approvals & autonomous actions\n\n");
    md.push_str(&format!(
        "growth: {} | outreach: {} | tools: {} — AUTONOMY (2026-07-16, commits \
         b6f148a/4febc11/7ea9c23): growth publish, outreach send, and self-tooling \
         validate+invoke run AUTONOMOUSLY behind automated gates — items below do NOT wait for an \
         operator `approved: true`; each carries the exact edit that STOPS it. Still human-gated: \
         money-out (money_guard, fail-closed), live-capital changes, growth_publish live-mode \
         promotion, SMTP credential provisioning, and external platform logins.\n\n",
        growth.len(),
        outreach.len(),
        tools.len()
    ));
    if growth.is_empty() && outreach.is_empty() && tools.is_empty() {
        md.push_str("Nothing pending.\n");
        return md;
    }
    if !growth.is_empty() {
        md.push_str("## Growth drafts (will auto-publish at next slow-tail unless removed)\n\n");
        for g in growth {
            md.push_str(&format!(
                "- **{}** — {}\n  - action: {}\n",
                g["lane"].as_str().unwrap_or(""),
                g["preview"].as_str().unwrap_or(""),
                g["action"].as_str().unwrap_or("")
            ));
        }
        md.push('\n');
    }
    if !outreach.is_empty() {
        md.push_str("## Outreach drafts (queued for autonomous send)\n\n");
        for o in outreach {
            md.push_str(&format!(
                "- **{}** — to {} / subj {}\n  - action: {}\n",
                o["lane"].as_str().unwrap_or(""),
                o["to"].as_str().unwrap_or("?"),
                o["subject"].as_str().unwrap_or("?"),
                o["action"].as_str().unwrap_or("")
            ));
        }
        md.push('\n');
    }
    if !tools.is_empty() {
        md.push_str("## Self-authored tools (autonomous — no operator approval gate)\n\n");
        for t in tools {
            let dry = if t["pending_validation"].as_bool().unwrap_or(false) {
                "pending".to_string()
            } else {
                t["dry_run_passed"].as_bool().unwrap_or(false).to_string()
            };
            md.push_str(&format!(
                "- **{}** — {} (lint={} dry_run={dry})\n  - action: {}\n",
                t["name"].as_str().unwrap_or(""),
                t["purpose"].as_str().unwrap_or(""),
                t["lint_passed"].as_bool().unwrap_or(false),
                t["action"].as_str().unwrap_or("")
            ));
        }
        md.push('\n');
    }
    md
}

// --------------------------------------------------------------------------- //
// regenerate — the every-tick surface writer (file-IO only, bounded, no LLM):
// every auto-publish-pending growth draft, queued outreach draft, and legacy
// (non-auto-registered) tool row, each with its truthful state + stop edit.
// --------------------------------------------------------------------------- //

/// Regenerate both surfaces from disk and return `{ok, counts:{growth,outreach,tools}}`. Reads the
/// newest drafts line per PUBLIC lane (bounded `tail(1)`, never a whole-file read) + the tooling
/// manifest; a corrupt log omits its lane (catch_unwind), a corrupt manifest reads as empty; both
/// output files are written atomically (temp + rename — a reader never sees a truncation). Writes
/// are best-effort: a disk hiccup loses one sweep's refresh, never the tick.
pub fn regenerate() -> Value {
    let mut growth_rows: Vec<Value> = Vec::new();
    let mut outreach_rows: Vec<Value> = Vec::new();
    for repo in registry::read_repos_json() {
        // Only explicit public lanes carry gated drafts (same cheap pre-check as the composers).
        if !repo.get("public").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let lane = paths::repo_name(&repo);
        let Some(dir) = paths::runtime_dir(&repo) else {
            continue;
        };
        for (file, sink) in [
            ("growth_drafts.jsonl", &mut growth_rows),
            ("outreach_drafts.jsonl", &mut outreach_rows),
        ] {
            let path = dir.join(file);
            // catch_unwind PER lane+log (edge E19): a corrupt log can never wedge the surface —
            // that one row is omitted and every other lane still renders.
            let newest = std::panic::catch_unwind(|| {
                ObservationLog::at(path.clone())
                    .tail(1)
                    .into_iter()
                    .next()
                    .unwrap_or_default()
            })
            .unwrap_or_default();
            if !newest.trim().is_empty() {
                sink.push(json!({
                    "lane": lane,
                    "path": path.to_string_lossy(),
                    "newest": newest,
                }));
            }
        }
    }
    // Corrupt/absent manifest reads as EMPTY (edge E16) — a malformed entry is omitted, not trusted.
    let manifest: Value = std::fs::read(tools_manifest_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}));

    let growth = collect_growth_pending(&growth_rows);
    let outreach = collect_outreach_pending(&outreach_rows);
    let tools = collect_tools_pending(&manifest);

    let md = render_md(&growth, &outreach, &tools);
    let payload = json!({
        "generated_ts": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "growth": growth,
        "outreach": outreach,
        "tools": tools,
    });
    if let Some(parent) = md_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_bytes(&md_path(), md.as_bytes());
    let _ = proc::atomic_write_json(&json_path(), &payload);
    json!({"ok": true, "counts": {
        "growth": growth.len(), "outreach": outreach.len(), "tools": tools.len(),
    }})
}

/// Test-only serialization for the two singleton output files: `regenerate` + the tick-wiring test
/// write the SAME `_pending_approvals.*` paths (and `atomic_write_bytes` uses a shared `<path>.tmp`),
/// so concurrent writers in one test binary could collide on the tmp rename. Mirrors NOTIFY_ENV_LOCK.
#[cfg(test)]
pub(crate) static APPROVALS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// --------------------------------------------------------------------------- //
// tests
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    fn growth_row(lane: &str, newest: &str) -> Value {
        json!({"lane": lane, "path": format!("runtime/{lane}/growth_drafts.jsonl"), "newest": newest})
    }

    fn outreach_row(lane: &str, newest: &str) -> Value {
        json!({"lane": lane, "path": format!("runtime/{lane}/outreach_drafts.jsonl"), "newest": newest})
    }

    // -------- collectors surface exactly what the autonomous seams would act on --------
    #[test]
    fn collect_growth_pending_surfaces_only_auto_publishable_content_lines() {
        let rows = vec![
            growth_row(
                "sover",
                "2026-07-16\trsi: growth DRAFT [GATED, unpublished, organic] lane=sover: x (t=1)",
            ),
            growth_row(
                "dotz",
                "2026-07-16\t{\"approved\": true, \"title\": \"ship it\"}",
            ),
            growth_row("", "2026-07-16\tnameless row is skipped (t=1)"),
            growth_row("blank", "   "),
        ];
        let out = collect_growth_pending(&rows);
        assert_eq!(
            out.len(),
            1,
            "only the publishable sover content line surfaces: {out:?}"
        );
        assert_eq!(out[0]["lane"], "sover");
        // the action states the AUTONOMOUS truth + the EXACT stop edit
        let action = out[0]["action"].as_str().unwrap();
        assert!(
            action.contains("will auto-publish at next slow-tail sweep"),
            "{action}"
        );
        assert!(action.contains("unless removed"), "{action}");
        assert!(
            action.contains("delete the newest line of runtime/sover/growth_drafts.jsonl"),
            "{action}"
        );
        // JSON control lines of ANY shape are not content — the seam refuses them, nothing imminent
        let control = vec![
            growth_row("a", "{\"approved\": \"true\"}"),
            growth_row("b", "{\"approved\": 1}"),
        ];
        assert!(collect_growth_pending(&control).is_empty());
        // a persona-violating draft will never auto-publish — never listed as imminent
        let persona = vec![growth_row(
            "c",
            "2026-07-16\tdraft credited to cayleb (t=1)",
        )];
        assert!(collect_growth_pending(&persona).is_empty());
    }

    #[test]
    fn collect_outreach_pending_resolves_to_and_subject() {
        let fact = "2026-07-16\trsi: outreach DRAFT [GATED, unsent, cold-email] lane=sover \
                    target=ab12cd34 to=person@org.com subj=A short honest intro :: body text here \
                    (t=1752 model=m prompt=ff00 rationale=fits)";
        let out = collect_outreach_pending(&[outreach_row("sover", fact)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["to"], "person@org.com");
        assert_eq!(out[0]["subject"], "A short honest intro");
        // the action states the AUTONOMOUS send truth + the outbox stop edit
        let action = out[0]["action"].as_str().unwrap();
        assert!(action.contains("auto-sends (no approval wait)"), "{action}");
        assert!(action.contains("operator target list"), "{action}");
        assert!(
            action.contains("runtime/sover/outreach_outbox.jsonl"),
            "{action}"
        );
        // a hand-edited JSON control line is not a queued composer draft — never surfaces
        let approved =
            "{\"approved\": true, \"to\": \"p@o.com\", \"subject\": \"s\", \"body\": \"b\"}";
        assert!(collect_outreach_pending(&[outreach_row("sover", approved)]).is_empty());
    }

    #[test]
    fn collect_tools_pending_surfaces_only_unapproved_entries() {
        let manifest = json!({"version": 1, "tools": {
            "zeta": {"purpose": "z", "approved": true,
                     "lint": {"passed": true}, "dry_run": {"passed": true}},
            "alpha": {"purpose": "count runtime files", "approved": false,
                      "lint": {"passed": true}, "dry_run": {"passed": true}},
            "beta": {"purpose": "no approved key at all",
                     "lint": {"passed": true}, "dry_run": {"passed": false}},
        }});
        let out = collect_tools_pending(&manifest);
        assert_eq!(
            out.len(),
            2,
            "approved:true is excluded, absent/false surface: {out:?}"
        );
        assert_eq!(out[0]["name"], "alpha"); // deterministic (sorted) order
        assert_eq!(out[1]["name"], "beta");
        assert_eq!(out[1]["dry_run_passed"], false);
        // dry-run-passed alpha invokes autonomously (approved is no longer a gate) — truth + retire
        let alpha = out[0]["action"].as_str().unwrap();
        assert!(alpha.contains("invokes AUTONOMOUSLY"), "{alpha}");
        assert!(alpha.contains("delete tools.alpha"), "{alpha}");
        // dry-run-failed beta is an inert stub, and says so
        let beta = out[1]["action"].as_str().unwrap();
        assert!(beta.contains("inert stub"), "{beta}");
        assert!(beta.contains("delete tools.beta"), "{beta}");
        // empty / malformed manifests collect to nothing, never a panic (edge E16)
        assert!(collect_tools_pending(&json!({})).is_empty());
        assert!(collect_tools_pending(&json!({"tools": []})).is_empty());
    }

    #[test]
    fn collect_tools_pending_states_the_post_autonomy_truth_per_dry_run_state() {
        let manifest = json!({"version": 1, "tools": {
            // legacy pre-autonomy stubs — their dry-run never ran/passed; nothing resurrects them
            "gamma": {"purpose": "new helper", "approved": false, "approved_validation": false,
                      "validation": "pending_operator",
                      "lint": {"passed": true}, "dry_run": {"passed": false, "pending": true}},
            "delta": {"purpose": "queued helper", "approved": false, "approved_validation": true,
                      "validation": "pending_operator",
                      "lint": {"passed": true}, "dry_run": {"passed": false, "pending": true}},
            // dry-run passed — invokes autonomously; the legacy approved:false does NOT hold it
            "epsilon": {"purpose": "validated helper", "approved": false,
                        "approved_validation": true, "validation": "validated",
                        "lint": {"passed": true}, "dry_run": {"passed": true}},
        }});
        let out = collect_tools_pending(&manifest);
        assert_eq!(out.len(), 3, "{out:?}");
        let by_name = |n: &str| out.iter().find(|t| t["name"] == n).unwrap().clone();
        for stub in ["gamma", "delta"] {
            let t = by_name(stub);
            assert_eq!(t["pending_validation"], true);
            let action = t["action"].as_str().unwrap();
            assert!(action.contains("inert stub"), "{action}");
            assert!(action.contains(&format!("delete tools.{stub}")), "{action}");
            // the old operator-edit instructions are GONE — they no longer unlock anything
            assert!(!action.contains("approved_validation\": true"), "{action}");
        }
        let epsilon = by_name("epsilon");
        let action = epsilon["action"].as_str().unwrap();
        assert!(action.contains("invokes AUTONOMOUSLY"), "{action}");
        assert!(action.contains("no longer consulted"), "{action}");
    }

    // -------- fact field extraction (pure) --------
    #[test]
    fn fact_field_and_subject_extraction() {
        let fact =
            "rsi: outreach DRAFT lane=x target=k1 to=a@b.c subj=Two word subject :: body (t=1)";
        assert_eq!(fact_field(fact, "to="), "a@b.c");
        assert_eq!(fact_field(fact, "target="), "k1");
        assert_eq!(fact_subject(fact), "Two word subject");
        // absent keys degrade to empty, never a panic
        assert_eq!(fact_field("no keys here", "to="), "");
        assert_eq!(fact_subject("no subject marker"), "");
    }

    // -------- render_md (pure golden) --------
    #[test]
    fn render_md_states_the_autonomy_truth_and_each_items_stop_edit() {
        let growth = collect_growth_pending(&[growth_row(
            "sover",
            "2026-07-16\trsi: growth DRAFT [GATED, unpublished, organic] lane=sover: reel copy (t=9)",
        )]);
        let outreach = collect_outreach_pending(&[outreach_row(
            "sover",
            "2026-07-16\trsi: outreach DRAFT [GATED, unsent, cold-email] lane=sover target=k \
             to=p@o.com subj=hello there :: hi (t=9 model=m prompt=h rationale=r)",
        )]);
        let tools = collect_tools_pending(&json!({"tools": {
            "counter": {"purpose": "count things", "approved": false,
                        "lint": {"passed": true}, "dry_run": {"passed": true}},
            "fresh": {"purpose": "unvalidated", "approved": false, "approved_validation": false,
                      "lint": {"passed": true}, "dry_run": {"passed": false, "pending": true}}}}));
        let md = render_md(&growth, &outreach, &tools);
        assert!(md.starts_with("# Pending approvals"), "{md}");
        assert!(md.contains("growth: 1 | outreach: 1 | tools: 2"), "{md}");
        // the header tells the AUTONOMY truth — and the pre-7/16 claim is gone
        assert!(
            md.contains("run AUTONOMOUSLY behind automated gates"),
            "{md}"
        );
        assert!(md.contains("Still human-gated: money-out"), "{md}");
        assert!(!md.contains("Solomon never self-approves"), "{md}");
        assert!(!md.contains("waits for an operator-set"), "{md}");
        // growth: auto-publish relabel + the stop edit
        assert!(
            md.contains("## Growth drafts (will auto-publish at next slow-tail unless removed)"),
            "{md}"
        );
        assert!(
            md.contains("delete the newest line of runtime/sover/growth_drafts.jsonl"),
            "{md}"
        );
        // outreach: autonomous send + the outbox stop edit
        assert!(
            md.contains("## Outreach drafts (queued for autonomous send)"),
            "{md}"
        );
        assert!(md.contains("to p@o.com / subj hello there"), "{md}");
        assert!(md.contains("runtime/sover/outreach_outbox.jsonl"), "{md}");
        // tools: no approval gate; the legacy stub keeps its honest dry_run=pending flag
        assert!(
            md.contains("## Self-authored tools (autonomous — no operator approval gate)"),
            "{md}"
        );
        assert!(md.contains("invokes AUTONOMOUSLY"), "{md}");
        assert!(md.contains("(lint=true dry_run=pending)"), "{md}");
        assert!(md.contains("delete tools.fresh"), "{md}");
        assert!(!md.contains("Nothing pending"), "{md}");
    }

    #[test]
    fn render_md_empty_is_an_honest_nothing_pending() {
        let md = render_md(&[], &[], &[]);
        assert!(md.contains("growth: 0 | outreach: 0 | tools: 0"), "{md}");
        assert!(md.contains("Nothing pending.\n"), "{md}");
        assert!(
            !md.contains("##"),
            "no fabricated sections on an empty surface: {md}"
        );
    }

    // -------- regenerate writes both surfaces (hermetic temp home) --------
    #[test]
    fn regenerate_writes_both_surfaces_and_returns_counts() {
        let _l = APPROVALS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let out = regenerate();
        assert_eq!(out["ok"], true, "{out}");
        // counts are present and numeric; their CORRECTNESS is pinned by the pure collector tests
        // above (this shared temp home may hold artifacts from parallel tests, so no exact zeros).
        for k in ["growth", "outreach", "tools"] {
            assert!(out["counts"][k].is_u64(), "counts.{k} present: {out}");
        }
        let md = std::fs::read_to_string(md_path()).expect("md surface written");
        assert!(md.starts_with("# Pending approvals"), "{md}");
        let j: Value =
            serde_json::from_slice(&std::fs::read(json_path()).expect("json surface written"))
                .expect("json surface parses");
        assert!(j.get("generated_ts").is_some());
        assert!(j.get("growth").map(|v| v.is_array()).unwrap_or(false));
        assert!(j.get("tools").map(|v| v.is_array()).unwrap_or(false));
    }
}
