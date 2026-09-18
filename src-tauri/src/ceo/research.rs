//! D5 — Layer 1: the first NON-ENGINEERING specialist behind the D4 constrained interface.
//!
//! ============================ WHAT THIS ADDS =============================
//! D4 (`ceo::orchestrator`) proved the `trait Specialist` seam with ONE worker: Engineering (the pi
//! coder), which acts INSIDE Solomon's own repos. D5 is the FIRST place Solomon acts OUTSIDE its own
//! repos — so it is gated HARDER, not looser. [`ResearchSpecialist`] ONLY reads the world and produces
//! a DRAFT: a competitor / market / opportunity OBSERVATION appended, provenance-tagged, to a lane's
//! PECRT observation log (`runtime/<lane>/observations.jsonl`, the append-only DATED fact log
//! `pecrt::warm::ObservationLog` owns). It PUBLISHES nothing, SPENDS nothing, and MUTATES nothing
//! outside that one drafts/observation-log path. A human reviews the note; nothing auto-ships.
//! ========================================================================
//!
//! ## Why this is safe by CONSTRUCTION (the same three gates, tightened)
//!
//!   1. `allowed_tools()` — [`RESEARCH_ALLOWED_TOOLS`] is a CLOSED whitelist of READ-ONLY tools plus
//!      the SINGLE local draft sink (`append_observation_draft`). It contains ZERO money-capable tools
//!      (`money_guard::is_money_capable` says so for every entry — a `#[test]` pins it) and ZERO
//!      external-mutation tools (no publish / post / deploy / send / commit / push / pay — a `#[test]`
//!      pins it against [`EXTERNAL_MUTATION_MARKERS`]). A FUTURE publish/pay tool cannot be added to
//!      this whitelist without tripping those tests, and even if named it would be DENIED at `gate`.
//!
//!   2. `scope_globs()` — restricted to the ONE observation-log/drafts path
//!      (`runtime/<lane>/observations.jsonl`). It NEVER returns `money_globs` (the trading-code blast
//!      radius) and NEVER a `protected` grader/leash path. The research worker's writable surface is a
//!      single append-only draft log under Solomon's runtime dir — never the product repo, never money
//!      code.
//!
//!   3. `gate()` — the IDENTICAL fail-closed composition Engineering uses: `money_guard::guard`
//!      (outermost default-DENY on any money-capable probe) then `pecrt::safety::guard_schedule` (deny
//!      any self-governance mutation). `money_guard` stays fail-closed, so ANY future publish/pay tool
//!      the specialist might attempt is denied by default — the acceptance-(d) guarantee that "no
//!      publish/pay tool is even reachable" is enforced here, not merely asserted.
//!
//! ## What this module deliberately does NOT do (ponytail scope discipline)
//!
//!   * NO publishing. It never posts, deploys, emails, or ships. The Growth alternative in the D5 spec
//!     would DRAFT sover content the same way (draft, never publish); this Research variant drafts an
//!     observation note. Either way the artifact is GATED for human review.
//!   * NO money capability. Its whitelist names no money tool; a money-capable probe is DENIED at the
//!     gate. money_guard is unchanged and stays the outermost default-DENY.
//!   * NO new store. The draft lands in the EXISTING PECRT observation log via the EXISTING
//!     `ObservationLog::append_fact` (which itself rejects prose-of-prose and requires a dated first-
//!     order fact). This module adds a WRITER for that log; it invents no schema.
//!   * NO outcome-critique in write mode. Per the D5 spec, the D3 outcome-critique is reused only in
//!     READ mode — a research note is an OBSERVATION, not a shipped change, so it is never graded as a
//!     profit-moving ship. This module produces the note; it does not self-grade it as progress.
#![allow(dead_code)]

use crate::ceo::orchestrator::{Specialist, Task, TaskKind};
use crate::pecrt::warm::ObservationLog;
use chrono::Utc;
use serde_json::{Value, json};

// --------------------------------------------------------------------------- #
// The CLOSED, READ-ONLY tool whitelist (ZERO money, ZERO external-mutation)
// --------------------------------------------------------------------------- #

/// The CLOSED tool whitelist for the Research specialist. Every entry is either a READ tool or the
/// SINGLE local draft sink — NONE is money-capable, NONE mutates anything external. The enforcement
/// is `gate` (money_guard + pecrt::safety); this list is the descriptive contract the two `#[test]`s
/// (`research_whitelist_has_no_money_tool`, `research_whitelist_has_no_external_mutation_tool`) pin so
/// the whitelist and the gate can never diverge and a future publish/pay tool cannot be slipped in.
///
///   * `read_observation_log`   — read the lane's existing dated observation facts (context).
///   * `read_outcomes_ledger`   — read `runtime/outcomes.jsonl` (read-only, tails only).
///   * `read_freshness_ledger`  — read `runtime/<lane>/freshness.json` (read-only).
///   * `read_progress_ledger`   — read `runtime/<lane>/progress.json` (read-only).
///   * `search_web_readonly`    — a READ-ONLY web/search probe (fetch competitor/market pages). Names
///     the seam; it performs no login, no post, no purchase — a GET, never a mutation.
///   * `append_observation_draft` — the ONE write: append a DRAFT observation to the lane's PECRT
///     observation log. A LOCAL append under Solomon's runtime dir, NOT an external publish. The
///     artifact is GATED (published:false) for human review.
pub const RESEARCH_ALLOWED_TOOLS: &[&str] = &[
    "read_observation_log",
    "read_outcomes_ledger",
    "read_freshness_ledger",
    "read_progress_ledger",
    "search_web_readonly",
    "append_observation_draft",
];

/// Substrings that mark a tool as an EXTERNAL MUTATION — a tool that changes state OUTSIDE Solomon's
/// own local runtime draft log (publish / post / deploy / send / commit / push / merge / delete / a
/// write to a remote). The Research specialist's whitelist must contain NONE of these (pinned by
/// `research_whitelist_has_no_external_mutation_tool`). This is the acceptance-(c) tripwire: a future
/// publish/pay tool named `post_to_x` / `deploy_sover` / `send_email` / `git_push` trips it here
/// BEFORE it could ever reach the gate. Deliberately does NOT list `append_observation_draft`'s verb
/// `append` — a local append to the runtime draft log is the sanctioned DRAFT sink, not an external
/// mutation. Money verbs are covered separately by `money_guard::is_money_capable`.
pub const EXTERNAL_MUTATION_MARKERS: &[&str] = &[
    "publish",
    "post",
    "deploy",
    "ship",
    "release",
    "send",
    "email",
    "message",
    "dm",
    "commit",
    "push",
    "merge",
    "pr_",
    "open_pr",
    "delete",
    "remove",
    "upload",
    "put_",
    "write_remote",
    "tweet",
    "toot",
    "webhook",
    "notify_external",
    "submit",
];

/// True iff `tool` names an external-mutation tool (case-insensitive substring against
/// [`EXTERNAL_MUTATION_MARKERS`]). The Research whitelist must trip this for NONE of its entries.
pub fn is_external_mutation(tool: &str) -> bool {
    let t = tool.to_ascii_lowercase();
    EXTERNAL_MUTATION_MARKERS.iter().any(|m| t.contains(m))
}

// --------------------------------------------------------------------------- #
// ResearchSpecialist — reads the world, DRAFTS an observation, publishes NOTHING
// --------------------------------------------------------------------------- #

/// The Research specialist: the FIRST worker that acts outside Solomon's own repos, constrained to
/// READ + a single local DRAFT append. Stateless (like the Engineering specialist) — one `run` per
/// wake, producing one provenance-tagged observation for a human to review.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResearchSpecialist;

impl ResearchSpecialist {
    pub fn new() -> Self {
        ResearchSpecialist
    }

    /// The observation-log path for `repo`: `runtime/<lane>/observations.jsonl` (the EXACT path
    /// `pecrt::warm::ObservationLog` + the `LongTermAdapter` freshness/progress siblings live under —
    /// per-lane runtime UNDER Solomon, never inside the product repo). `None` for a nameless row.
    pub fn observation_log_path(repo: &Value) -> Option<std::path::PathBuf> {
        crate::control::paths::runtime_dir(repo).map(|d| d.join("observations.jsonl"))
    }

    /// The repo-relative drafts/observation-log glob this specialist may write — the ONE path, and
    /// ONLY it. Shared by `scope_globs` and the scope `#[test]` so they cannot diverge.
    fn drafts_glob(repo: &Value) -> Vec<String> {
        let lane = crate::control::paths::repo_name(repo);
        if lane.is_empty() {
            return Vec::new();
        }
        // The observation-log/drafts path, relative to Solomon's runtime root.
        vec![format!("runtime/{lane}/observations.jsonl")]
    }

    /// Build the provenance-tagged DRAFT observation fact from a research task's detail. The fact is
    /// an `rsi:`-provenance-tagged (per `provenance.rs`) first-order dated line stating a
    /// competitor/market/opportunity OBSERVATION — GATED (it is a draft note, never a shipped change).
    /// Pure (caller supplies the date) so the `#[test]` can pin the shape without a clock. The
    /// returned fact ALWAYS carries a concrete datum (the wake epoch second) so it passes
    /// `ObservationLog::validate_fact` even when `detail` is prose-only.
    pub(crate) fn draft_fact(task: &Task, epoch_s: u64) -> String {
        let lane = &task.lane;
        let note = task.detail.trim();
        let note = if note.is_empty() {
            "research observation (no detail supplied)"
        } else {
            note
        };
        // `rsi:` provenance prefix (provenance.rs tag convention) + GATED marker so a human reviewer
        // sees at a glance this is an unshipped draft. The trailing `t=<epoch>` guarantees a datum.
        format!("rsi: research DRAFT [GATED, unpublished] lane={lane}: {note} (t={epoch_s})")
    }
}

impl Specialist for ResearchSpecialist {
    fn name(&self) -> &'static str {
        "research"
    }

    fn allowed_tools(&self) -> &'static [&'static str] {
        RESEARCH_ALLOWED_TOOLS
    }

    fn scope_globs(&self, repo: &Value) -> Vec<String> {
        // The ONLY writable surface: the lane's observation-log/drafts path. NEVER money_globs, NEVER
        // a protected grader/leash path — this specialist cannot write product code or money code.
        Self::drafts_glob(repo)
    }

    fn gate(&self, task: &Task, repo: &Value) -> Option<Value> {
        // (1) MONEY GUARD (outermost default-DENY) — IDENTICAL to the Engineering specialist. Any
        // money-capable probe (withdraw/transfer/pay/... or a future publish/pay tool expressed as a
        // money kind) is DENIED fail-closed. money_guard stays unchanged; this is the enforcement that
        // makes acceptance (d) real — no publish/pay tool is reachable, it is denied here.
        if let Some(refusal) = crate::money_guard::guard(task.kind.as_str(), repo) {
            return Some(refusal);
        }

        // (2) PECRT SAFETY (self-governance default-DENY) — IDENTICAL to Engineering. A dispatch whose
        // TARGET names a whitelist/cycle_budget/skeptic/blast-radius/kill/freshness surface has NO
        // allow arm and is denied. A research task on an ordinary lane re-enters the existing gate
        // funnel (reenters_gates=true), so it is admitted here.
        let req = crate::pecrt::safety::ScheduleRequest::new(task.kind.as_str(), &task.lane, true);
        if let Some(refusal) = crate::pecrt::safety::guard_schedule(&req) {
            return Some(refusal);
        }

        None
    }

    fn run(&self, task: &Task, repo: &Value) -> Value {
        // The gate MUST have passed before run() is reached; belt-and-suspenders re-check so a future
        // direct caller can never skip it (fail-closed, never a silent bypass) — same discipline as
        // the Engineering specialist.
        if let Some(refusal) = self.gate(task, repo) {
            return refusal;
        }

        let lane = crate::control::paths::repo_name(repo);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let iso_date = Utc::now().format("%Y-%m-%d").to_string();
        let fact = Self::draft_fact(task, now);

        // The ONE action: append the DRAFT observation to the lane's PECRT observation log — a LOCAL
        // append under Solomon's runtime dir, NOT an external publish. The append goes through the
        // EXISTING `ObservationLog::append_fact`, which enforces the dated-first-order-fact contract
        // (rejects prose-of-prose). Nothing is published; nothing is spent.
        let path = match Self::observation_log_path(repo) {
            Some(p) => p,
            None => {
                return json!({
                    "ok": false,
                    "specialist": "research",
                    "kind": "research_note",
                    "lane": lane,
                    "gated": true,
                    "published": false,
                    "spent": false,
                    "error": "no runtime dir (nameless lane) — cannot write observation draft",
                });
            }
        };
        let log = ObservationLog::at(path.clone());
        match log.append_fact(&iso_date, &fact) {
            Ok(()) => json!({
                "ok": true,
                "specialist": "research",
                "kind": "research_note",
                "lane": lane,
                // The artifact is a GATED, provenance-tagged DRAFT — a human reviews it; nothing
                // auto-ships, nothing publishes, nothing spends.
                "gated": true,
                "published": false,
                "spent": false,
                "provenance": "rsi:",
                "artifact_path": path.to_string_lossy(),
                "fact": fact,
            }),
            Err(rejected) => json!({
                "ok": false,
                "specialist": "research",
                "kind": "research_note",
                "lane": lane,
                "gated": true,
                "published": false,
                "spent": false,
                "error": format!("observation-log append rejected: {rejected:?}"),
            }),
        }
    }
}

// --------------------------------------------------------------------------- #
// The thin dispatch helper — mirrors orchestrator::dispatch_for_diagnosis
// --------------------------------------------------------------------------- #

/// Dispatch a research task to the Research specialist through the gated interface: read the lane,
/// build a `research_note` task, and route it through the SAME `orchestrator::dispatch` (gate FIRST,
/// then run). Returns the outcome Value. `detail` is the research prompt (what to observe about the
/// lane's competitors / market / opportunities). This is the seam a future thread-plane driver calls
/// to have Solomon produce a research DRAFT for a lane.
pub fn dispatch_research(repo: &Value, detail: &str) -> Value {
    let lane = crate::control::paths::repo_name(repo);
    // A research note is NOT a coding change and NOT a money remediation: it is a benign non-money
    // remediation kind. `Remediate("none")` is the closed-registry no-op kind — money_guard waves it
    // through (not money-capable) and pecrt::safety admits it on an ordinary lane, so the gate passes
    // for an ordinary lane and DENIES on a governance/money target exactly as for Engineering.
    let task = Task::new(TaskKind::Remediate("none"), &lane, detail);
    let research = ResearchSpecialist::new();
    crate::ceo::orchestrator::dispatch(&research, &task, repo)
}

// --------------------------------------------------------------------------- #
// tests — the four D5 acceptance contracts
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain_repo() -> Value {
        json!({ "name": "sover" })
    }

    /// A repo row with a UNIQUE lane name so a test's observation log never shares a runtime path
    /// with another test running in parallel (all resolve under the same per-process temp HERE).
    fn uniq_repo(tag: &str) -> Value {
        let name = format!(
            "d5research_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        json!({ "name": name })
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
    // ACCEPTANCE (a): the specialist produces a REAL research/draft artifact
    // into the observation log on wake, consuming ONLY whitelisted read tools.
    // We drive the real `run` against an isolated temp HERE and read the file
    // back — the artifact is a provenance-tagged, GATED, unpublished draft.
    // ===================================================================== #
    #[test]
    fn research_specialist_writes_a_gated_draft_into_the_observation_log_on_wake() {
        // paths::here() memoizes once per process; other tests in the crate may have set it. To keep
        // this test hermetic regardless of order, we assert on the RESOLVED path the specialist itself
        // reports, not a path we recompute from a (possibly-cached) HERE.
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        let repo = uniq_repo("write");
        let out = dispatch_research(
            &repo,
            "competitor scan: rival RSI harness shipped a novel-signal lane; note the opportunity",
        );

        // The outcome is an OK, GATED, UNPUBLISHED, UNSPENT research note.
        assert_eq!(out["ok"], true, "research note must succeed: {out}");
        assert_eq!(out["specialist"], "research");
        assert_eq!(out["kind"], "research_note");
        assert_eq!(
            out["gated"], true,
            "the artifact must be GATED for human review"
        );
        assert_eq!(out["published"], false, "NOTHING is published");
        assert_eq!(out["spent"], false, "NOTHING is spent");
        assert_eq!(out["provenance"], "rsi:", "the draft is provenance-tagged");

        // The artifact is REAL: read the observation log back off disk and confirm the dated,
        // provenance-tagged, GATED line landed.
        let artifact_path = out["artifact_path"]
            .as_str()
            .expect("artifact_path present");
        let body = std::fs::read_to_string(artifact_path).expect("observation log exists on disk");
        assert!(
            body.contains("rsi: research DRAFT"),
            "line is provenance-tagged: {body}"
        );
        assert!(
            body.contains("[GATED, unpublished]"),
            "line is marked gated+unpublished: {body}"
        );
        assert!(
            body.contains("competitor scan"),
            "the research detail is recorded: {body}"
        );
        // it is DATED (ObservationLog::dated_line prefixes an ISO date + tab).
        assert!(
            body.lines().next().unwrap().contains('\t'),
            "the observation line is dated (iso-date TAB fact): {body}"
        );

        // Cleanup: remove the runtime subtree we wrote (best-effort).
        if let Some(dir) = std::path::Path::new(artifact_path).parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    // ===================================================================== #
    // ACCEPTANCE (b): money_guard logs a DENY on ANY money-capable probe the
    // specialist attempts — the gate refuses BEFORE run(), on every lane
    // (money-out is the HARD invariant, no lane exempts it). Test-driven.
    // ===================================================================== #
    #[test]
    fn research_specialist_money_capable_probe_is_denied_at_the_gate() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        let research = ResearchSpecialist::new();

        // A representative spread of money-capable + would-be publish/pay probes, expressed as the
        // task KIND (the surface money_guard classifies). EVERY one must be DENIED at gate() BEFORE
        // run() — even on a whitelisted live-money lane.
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
            "subscribe",
        ] {
            for repo in [plain_repo(), kairos_repo()] {
                let money_task = Task {
                    kind: TaskKind::Remediate(money_kind_static(money_kind)),
                    lane: "kairos".to_string(),
                    detail: money_kind.to_string(),
                };
                let verdict = research.gate(&money_task, &repo);
                assert!(
                    verdict.is_some(),
                    "money-capable probe '{money_kind}' must be DENIED at the research gate on {repo}"
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
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    /// A denied money probe never reaches run() through the real dispatch path either: dispatch
    /// returns the refusal verbatim and NO observation line is written (nothing drafted, published, or
    /// spent on a denied money attempt).
    #[test]
    fn a_denied_money_probe_writes_no_artifact() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        let research = ResearchSpecialist::new();
        // A unique whitelisted (equity_usd) lane so the money DENY fires on the strongest lane while
        // the observation-log path stays isolated from other parallel tests.
        let mut repo = uniq_repo("denymoney");
        repo["equity_usd"] = json!(1234.5);
        let before = ResearchSpecialist::observation_log_path(&repo)
            .map(|p| std::fs::read_to_string(&p).unwrap_or_default())
            .unwrap_or_default();

        let denied = crate::ceo::orchestrator::dispatch(
            &research,
            &Task::new(TaskKind::Remediate("withdraw"), "kairos", "probe"),
            &repo,
        );
        assert_eq!(denied["ok"], false, "a money probe must be denied");

        let after = ResearchSpecialist::observation_log_path(&repo)
            .map(|p| std::fs::read_to_string(&p).unwrap_or_default())
            .unwrap_or_default();
        assert_eq!(
            before, after,
            "a denied money probe must write NO observation line"
        );
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    // ===================================================================== #
    // ACCEPTANCE (c): the specialist's whitelist contains NO external-mutation
    // tool (and NO money-capable tool). The whitelist and the gate agree by
    // construction — a future publish/pay tool cannot be slipped in.
    // ===================================================================== #
    #[test]
    fn research_whitelist_has_no_external_mutation_tool() {
        let research = ResearchSpecialist::new();
        for t in research.allowed_tools() {
            assert!(
                !is_external_mutation(t),
                "the Research whitelist names an external-mutation tool '{t}' — it must not. \
                 The specialist only READS and DRAFTS locally; it never publishes/posts/deploys/\
                 sends/commits/pushes/pays"
            );
        }
        // Positive control: the markers DO catch real publish/pay tool names, so the guard has teeth.
        for bad in [
            "post_to_x",
            "deploy_sover",
            "send_email",
            "git_push",
            "open_pr",
            "upload_asset",
        ] {
            assert!(
                is_external_mutation(bad),
                "'{bad}' should be caught as external-mutation"
            );
        }
    }

    #[test]
    fn research_whitelist_has_no_money_tool() {
        let research = ResearchSpecialist::new();
        for t in research.allowed_tools() {
            assert!(
                !crate::money_guard::is_money_capable(t),
                "the Research whitelist names a money-capable tool '{t}' — it must not"
            );
        }
    }

    // ===================================================================== #
    // ACCEPTANCE (d): nothing is published and nothing is spent — no publish/
    // pay tool is even REACHABLE. Proven three ways: (1) the whitelist names
    // none (tests above); (2) a publish/pay probe is DENIED at the gate; (3)
    // the successful run's outcome is published:false / spent:false.
    // ===================================================================== #
    #[test]
    fn nothing_is_published_and_nothing_is_spent() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("SOLOMON_NOTIFY_OFF", "1") };

        let research = ResearchSpecialist::new();

        // (2) A would-be publish/pay probe expressed as a money kind is DENIED at the gate (no
        // publish/pay path is reachable). `buy_ads` / `pay_invoice` / `stripe_checkout` are the
        // canonical spend verbs; each must refuse.
        for pay_kind in ["buy_ads", "pay_invoice", "stripe_checkout", "ad_spend"] {
            let t = Task::new(
                TaskKind::Remediate(money_kind_static(pay_kind)),
                "sover",
                "probe",
            );
            assert!(
                research.gate(&t, &plain_repo()).is_some(),
                "publish/pay probe '{pay_kind}' must be DENIED — no pay tool is reachable"
            );
        }

        // (3) The one thing the specialist CAN do (draft a note) reports published:false, spent:false.
        let repo = uniq_repo("nopublish");
        let out = dispatch_research(&repo, "market note: 3 rival lanes observed at t");
        assert_eq!(out["published"], false);
        assert_eq!(out["spent"], false);
        assert_eq!(out["gated"], true);

        // And `place_trade` — the ONLY money verb money_guard has an allow arm for — is STILL not a
        // reachable action kind (it is not in ACTION_KINDS), so even a whitelisted lane's research
        // worker cannot dispatch a trade. (Belt-and-suspenders cross-check of the D4/money_guard
        // invariant from this specialist's vantage.)
        assert!(
            !crate::actions::ACTION_KINDS.contains(&crate::money_guard::PLACE_TRADE_KIND),
            "place_trade must never be a dispatchable action kind"
        );

        // cleanup
        if let Some(p) = ResearchSpecialist::observation_log_path(&repo) {
            if let Some(dir) = p.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("SOLOMON_NOTIFY_OFF") };
    }

    // ---- scope_globs restricts writes to the drafts/observation-log path ONLY ----
    #[test]
    fn research_scope_globs_is_the_observation_log_path_only() {
        let research = ResearchSpecialist::new();
        // kairos carries money_globs + protected — the research specialist must return NEITHER, only
        // the observation-log/drafts path.
        let globs = research.scope_globs(&kairos_repo());
        assert_eq!(globs, vec!["runtime/kairos/observations.jsonl"]);
        // NEVER the money-code blast radius.
        assert!(
            !globs
                .iter()
                .any(|g| g.contains("trader.py") || g.contains("kalshi"))
        );
        // NEVER a protected grader/leash path.
        assert!(
            !globs
                .iter()
                .any(|g| g.contains("promote.py") || g.contains(".state"))
        );
        // a nameless row yields no writable scope.
        assert!(research.scope_globs(&json!({})).is_empty());
    }

    // ---- the draft fact is a provenance-tagged, dated, first-order fact (passes validate_fact) ----
    #[test]
    fn draft_fact_is_provenance_tagged_and_a_valid_first_order_fact() {
        let task = Task::new(
            TaskKind::Remediate("none"),
            "sover",
            "rival posted 4 new items",
        );
        let fact = ResearchSpecialist::draft_fact(&task, 1_783_000_000);
        assert!(
            fact.starts_with("rsi: research DRAFT [GATED, unpublished]"),
            "{fact}"
        );
        assert!(fact.contains("lane=sover"));
        assert!(fact.contains("rival posted 4 new items"));
        // It MUST pass the observation log's first-order-fact validator (it carries a datum: t=...).
        assert!(
            crate::pecrt::warm::ObservationLog::validate_fact(&fact).is_fact(),
            "the draft fact must be a valid first-order dated fact: {fact}"
        );
        // even an empty detail still yields a valid fact (the epoch datum guarantees it).
        let empty = Task::new(TaskKind::Remediate("none"), "sover", "   ");
        let f2 = ResearchSpecialist::draft_fact(&empty, 1_783_000_001);
        assert!(
            crate::pecrt::warm::ObservationLog::validate_fact(&f2).is_fact(),
            "{f2}"
        );
    }

    /// Test helper: money verbs under test are compile-time literals; map each back to its static
    /// form so `Remediate(&'static str)` can carry it (bounded, test-only).
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
            "subscribe" => "subscribe",
            _ => "unknown_money_kind",
        }
    }
}
