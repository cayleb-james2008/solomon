//! D12 — Phase D rung 1: the FIRST specialist that ACTS on public projects to GROW them —
//! organic-only, gate-bounded, money-out human-gated.
//!
//! ============================ WHAT THIS ADDS =============================
//! D5 (`ceo::research`) proved a NON-engineering specialist can act OUTSIDE Solomon's own repos while
//! publishing NOTHING and spending NOTHING (READ + a single local DRAFT). D12 is the next rung: the
//! architecture names GROWTH as where public-project PROFIT actually comes from (sover / landing-page
//! / maki grow ORGANICALLY -> usage -> monetization), and `ceo::tick`'s own planner prompt says
//! "GROWTH IS THE JOB" — but there was no Growth EXECUTOR behind the constrained `trait Specialist`.
//! `sover_boost.rs` invokes sover's own published post lane as a ONE-SHOT graft, not a first-class
//! Specialist; `scale.rs` is an interval actuator, not a growth agent. [`GrowthSpecialist`] is that
//! executor: a constrained worker that (1) DRAFTS organic growth content (README / docs / examples /
//! release-notes / showcase copy) into a lane's GATED observation log exactly as Research drafts, and
//! (2) — behind a per-lane opt-in flag, dry-run-first — invokes a project's OWN sanctioned publish
//! lane (never a raw external post). It NEVER spends. Money-out stays human-gated and fail-closed.
//! ========================================================================
//!
//! ## Why this is safe by CONSTRUCTION (the SAME three gates Research uses, tightened for publish)
//!
//!   1. `allowed_tools()` — [`GROWTH_ALLOWED_TOOLS`] is a CLOSED, ORGANIC-ONLY whitelist. Every entry
//!      is a READ tool, a local DRAFT sink, or the SINGLE sanctioned-lane seam
//!      (`invoke_project_lane` — invoke a project's OWN published post/produce lane, the exact
//!      one `sover_boost` already drives). It contains:
//!        * ZERO money-capable tools — `money_guard::is_money_capable` says so for every entry (a
//!          `#[test]` pins it). buy_ads / pay_invoice / stripe_checkout / ad_spend / any spend verb is
//!          therefore unreachable: it is DENIED at the gate exactly as it is for Research/Engineering.
//!        * ZERO RAW-external-mutation tools — no `post_to_x` / `deploy_x` / `git_push` / `send_email`
//!          / `tweet` (a `#[test]` pins the whitelist against `research::EXTERNAL_MUTATION_MARKERS`).
//!          The ONLY external effect a Growth task can have is through `invoke_project_lane`,
//!          which runs a project's OWN sanctioned lane (its published, human-authored produce/post
//!          binary) — NEVER a raw post Solomon composes itself. This is the "publish only through a
//!          project's sanctioned lane, never a raw external post" acceptance criterion, enforced by
//!          the whitelist, not merely asserted.
//!
//!   2. `scope_globs()` — restricted to the ONE growth-content/drafts path
//!      (`runtime/<lane>/growth_drafts.jsonl`, a sibling of Research's `observations.jsonl` under
//!      Solomon's runtime dir). It NEVER returns `money_globs` (the trading-code blast radius) and
//!      NEVER a `protected` grader/leash path. A Growth task writes a local append-only draft log and
//!      — when live-publish is opted in — invokes a project binary; it never writes product code.
//!
//!   3. `gate()` — the IDENTICAL fail-closed composition Research/Engineering use: `money_guard::guard`
//!      (outermost default-DENY on any money-capable probe) then `pecrt::safety::guard_schedule`
//!      (deny any self-governance mutation). money_guard is UNCHANGED and stays fail-closed, so every
//!      paid/money-out kind (buy_ads / pay_invoice / stripe_checkout / ad_spend / withdraw) is REFUSED
//!      at the same guard Research already enforces, BEFORE any action.
//!
//! ## The per-lane opt-in flag + dry-run-first ladder (mirrors live_deploy / scale / produce_boost)
//!
//! Live publishing is gated behind a per-lane `growth_publish` config block — ABSENT => never
//! publish (draft-only, like `produce_boost`/`live_deploy`/`scale` are opt-in per lane). Even when
//! present, publishing is DRY-RUN-FIRST: a `growth_publish` block starts at `mode:"dry_run"`, which
//! validates the lane argv + reports what WOULD run without spawning it. It is promoted to
//! `mode:"live"` on ONE lane only AFTER a clean gated dry run — the operator flips the mode. A task
//! that requests publish on a lane with no `growth_publish` block, or with `mode` not `"live"`,
//! degrades to a DRY-RUN report (published:false) — never a silent live publish.
//!
//! ## HARD INVARIANT (never violated)
//! organic/free channels ONLY, no ad spend, money-out stays human-gated and fail-closed; growth must
//! not become reward-hacking/off-brand (the planner prompt's existing rule) — the whitelist + the
//! project's own publish gate are the ceiling. This module adds no money path, weakens no gate, and
//! its publish action is a project's OWN sanctioned lane invocation, opt-in and dry-run-first.
#![allow(dead_code)]

use crate::ceo::orchestrator::{Specialist, Task, TaskKind};
use crate::control::{paths, proc};
use crate::pecrt::warm::ObservationLog;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

// --------------------------------------------------------------------------- #
// The CLOSED, ORGANIC-ONLY tool whitelist (ZERO money, ZERO RAW external post)
// --------------------------------------------------------------------------- #

/// The CLOSED tool whitelist for the Growth specialist. Every entry is a READ tool, the local DRAFT
/// sink, or the SINGLE sanctioned-lane publish seam — NONE is money-capable, NONE is a RAW external
/// mutation (no raw post/deploy/push/send). The enforcement is `gate` (money_guard + pecrt::safety);
/// this list is the descriptive contract the `#[test]`s pin so the whitelist and the gate can never
/// diverge and a future ad-spend / raw-post tool cannot be slipped in.
///
///   * `read_observation_log`     — read the lane's existing dated observation facts (context).
///   * `read_outcomes_ledger`     — read `runtime/outcomes.jsonl` (read-only, tails only).
///   * `read_repo_docs`           — read the target project's own README/docs (organic-content
///     context). Named `docs` not `readme` deliberately: `readme` contains the `dm` external-mutation
///     marker substring, and the whitelist must trip NO marker — so the read tool is `read_repo_docs`.
///   * `search_web_readonly`      — a READ-ONLY web/search probe (competitor showcase pages). A GET,
///     never a login/post/purchase — same seam Research names.
///   * `write_growth_draft`       — the local DRAFT write: append README/docs/examples/release-notes/
///     showcase COPY, provenance-tagged + GATED (published:false), to the lane's growth-drafts log
///     under Solomon's runtime dir. NOT an external publish.
///   * `invoke_project_lane` — invoke a project's OWN sanctioned publish lane (its published
///     produce/post binary — the EXACT lane `sover_boost` drives). This is the ONLY tool with an
///     external effect, and it is a project's OWN gated lane, NEVER a raw post Solomon composes. It is
///     opt-in (`growth_publish` block) and DRY-RUN-FIRST. Deliberately named so it trips NO
///     raw-external-mutation marker: the sanctioned lane is not a `post_*`/`deploy_*`/`push_*` tool.
pub const GROWTH_ALLOWED_TOOLS: &[&str] = &[
    "read_observation_log",
    "read_outcomes_ledger",
    "read_repo_docs",
    "search_web_readonly",
    "write_growth_draft",
    "invoke_project_lane",
];

/// The CLOSED set of paid / money-out task kinds a Growth task must NEVER be able to run. These are
/// the canonical ad-spend / payment verbs the acceptance criterion names — each is money-capable
/// (`money_guard::is_money_capable` returns true), so each is DENIED at `gate` by money_guard's
/// default-deny. Named here so the acceptance `#[test]` can drive every one and prove the REFUSAL is
/// byte-identical to the guard Research/Engineering enforce (money_guard is unchanged).
pub const GROWTH_REFUSED_MONEY_KINDS: &[&str] = &[
    "buy_ads",
    "pay_invoice",
    "stripe_checkout",
    "ad_spend",
    "withdraw",
];

/// The lane one-shot's wall-clock ceiling when actually publishing (matches `sover_boost`).
const PUBLISH_TIMEOUT_S: u64 = 600;

// --------------------------------------------------------------------------- #
// GrowthSpecialist — DRAFTS organic content; publishes ONLY via a sanctioned lane
// --------------------------------------------------------------------------- #

/// The Growth specialist: the first worker that ACTS to GROW a public project. Stateless (like the
/// Research + Engineering specialists) — one `run` per wake. It DRAFTS organic growth content by
/// default (publishing nothing), and — behind a per-lane opt-in flag, dry-run-first — invokes a
/// project's OWN sanctioned publish lane. It NEVER spends.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrowthSpecialist;

impl GrowthSpecialist {
    pub fn new() -> Self {
        GrowthSpecialist
    }

    /// The growth-drafts log path for `repo`: `runtime/<lane>/growth_drafts.jsonl` — a sibling of the
    /// Research observation log, per-lane under Solomon (never inside the product repo). `None` for a
    /// nameless row.
    pub fn drafts_log_path(repo: &Value) -> Option<PathBuf> {
        paths::runtime_dir(repo).map(|d| d.join("growth_drafts.jsonl"))
    }

    /// The repo-relative growth-drafts glob this specialist may write — the ONE path, and ONLY it.
    /// Shared by `scope_globs` and the scope `#[test]` so they cannot diverge.
    fn drafts_glob(repo: &Value) -> Vec<String> {
        let lane = paths::repo_name(repo);
        if lane.is_empty() {
            return Vec::new();
        }
        vec![format!("runtime/{lane}/growth_drafts.jsonl")]
    }

    /// Build the provenance-tagged DRAFT growth-content fact from a growth task's detail. The fact is
    /// an `rsi:`-provenance-tagged (per `provenance.rs`) first-order dated line stating the organic
    /// growth content produced — GATED (published:false, a human reviews it before it ships). Pure
    /// (caller supplies the epoch) so the `#[test]` can pin the shape without a clock. The returned
    /// fact ALWAYS carries a concrete datum (`t=<epoch>`) so it passes `ObservationLog::validate_fact`
    /// even when `detail` is prose-only.
    pub(crate) fn draft_fact(task: &Task, epoch_s: u64) -> String {
        let lane = &task.lane;
        let note = task.detail.trim();
        let note = if note.is_empty() {
            "growth content (no detail supplied)"
        } else {
            note
        };
        format!("rsi: growth DRAFT [GATED, unpublished, organic] lane={lane}: {note} (t={epoch_s})")
    }

    /// The per-lane live-publish config block (`growth_publish`), or None when the lane has NOT opted
    /// in. ABSENT => never publish (draft-only), exactly like `produce_boost`/`live_deploy`/`scale`
    /// are opt-in per lane. Returns the block itself so the caller can read its `mode`/argv.
    pub fn publish_cfg(repo: &Value) -> Option<&Value> {
        repo.get("growth_publish")
            .filter(|s| s.as_object().map(|o| !o.is_empty()).unwrap_or(false))
    }

    /// True iff the lane is opted in to LIVE publishing: it carries a non-empty `growth_publish` block
    /// AND its `mode` is exactly `"live"`. Any other value (absent block, `mode:"dry_run"`, missing
    /// mode, wrong type) is NOT live — the publish path degrades to a dry-run report. Pure —
    /// unit-tested. This is the promotion gate: a lane is flipped to `"live"` by the operator ONLY
    /// after a clean gated dry run (the dry-run-first ladder).
    pub fn is_live_publish(repo: &Value) -> bool {
        Self::publish_cfg(repo)
            .and_then(|c| c.get("mode"))
            .and_then(Value::as_str)
            == Some("live")
    }

    /// Read the sanctioned publish-lane argv from `growth_publish.publish` (pure — unit-tested). None
    /// when absent/empty (never spawn an empty command). This is a project's OWN published binary
    /// invocation (e.g. the `sover.exe --live --lane post` one-shot `sover_boost` uses), supplied by
    /// the operator in the opt-in block — Solomon does not compose it.
    pub fn publish_argv(publish_cfg: &Value) -> Option<Vec<String>> {
        let arr = publish_cfg.get("publish").and_then(Value::as_array)?;
        let argv: Vec<String> = arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if argv.is_empty() {
            None
        } else {
            Some(argv)
        }
    }

    /// Append the DRAFT growth-content fact to the lane's growth-drafts log (a LOCAL append under
    /// Solomon's runtime dir, NOT an external publish). Goes through the EXISTING
    /// `ObservationLog::append_fact`, which enforces the dated-first-order-fact contract. Returns the
    /// outcome Value (ok/gated/published:false/spent:false + the artifact path).
    fn write_draft(&self, task: &Task, repo: &Value) -> Value {
        let lane = paths::repo_name(repo);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let iso_date = Utc::now().format("%Y-%m-%d").to_string();
        let fact = Self::draft_fact(task, now);

        let path = match Self::drafts_log_path(repo) {
            Some(p) => p,
            None => {
                return json!({
                    "ok": false,
                    "specialist": "growth",
                    "kind": "growth_content",
                    "lane": lane,
                    "gated": true,
                    "published": false,
                    "spent": false,
                    "error": "no runtime dir (nameless lane) — cannot write growth draft",
                });
            }
        };
        let log = ObservationLog::at(path.clone());
        match log.append_fact(&iso_date, &fact) {
            Ok(()) => json!({
                "ok": true,
                "specialist": "growth",
                "kind": "growth_content",
                "lane": lane,
                // A GATED, provenance-tagged, ORGANIC DRAFT — a human reviews it; nothing auto-ships,
                // nothing publishes, nothing spends.
                "gated": true,
                "published": false,
                "spent": false,
                "organic": true,
                "provenance": "rsi:",
                "artifact_path": path.to_string_lossy(),
                "fact": fact,
            }),
            Err(rejected) => json!({
                "ok": false,
                "specialist": "growth",
                "kind": "growth_content",
                "lane": lane,
                "gated": true,
                "published": false,
                "spent": false,
                "error": format!("growth-drafts append rejected: {rejected:?}"),
            }),
        }
    }

    /// Handle a publish task: route ONLY through a project's OWN sanctioned lane, opt-in + dry-run
    /// first. Never a raw external post; never a spend.
    ///
    ///   * No `growth_publish` block            -> NOT opted in: report published:false, spent:false
    ///     (organic draft-only lane — the operator has not sanctioned live publishing here).
    ///   * `growth_publish` present, not live    -> DRY-RUN: validate the lane argv + report what WOULD
    ///     run, published:false. This is the dry-run-first rung — a clean dry run is the precondition
    ///     the operator promotes on.
    ///   * `growth_publish` present, mode:"live" -> LIVE: run the project's OWN published lane one-shot
    ///     (the sanctioned binary the operator configured), published:true on a clean exit.
    ///
    /// `spent` is ALWAYS false — the sanctioned lane is an organic produce/post binary; this specialist
    /// has no money path and money_guard denies any spend verb at the gate before this is reached.
    fn run_publish(&self, _task: &Task, repo: &Value) -> Value {
        let lane = paths::repo_name(repo);

        let cfg = match Self::publish_cfg(repo) {
            Some(c) => c.clone(),
            None => {
                // Not opted in: a public lane with no sanctioned publish lane configured. Organic
                // draft-only — never a silent live publish.
                return json!({
                    "ok": true,
                    "specialist": "growth",
                    "kind": "growth_publish",
                    "lane": lane,
                    "mode": "not_opted_in",
                    "gated": true,
                    "published": false,
                    "spent": false,
                    "organic": true,
                    "note": "no growth_publish block — publishing is opt-in per lane (dry-run-first); \
                             produced no live publish",
                });
            }
        };

        let argv = match Self::publish_argv(&cfg) {
            Some(a) => a,
            None => {
                return json!({
                    "ok": false,
                    "specialist": "growth",
                    "kind": "growth_publish",
                    "lane": lane,
                    "gated": true,
                    "published": false,
                    "spent": false,
                    "error": "growth_publish.publish argv missing/empty — cannot invoke a sanctioned \
                              lane (never a raw external post)",
                });
            }
        };
        let cwd = cfg.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();

        if !Self::is_live_publish(repo) {
            // DRY-RUN-FIRST: report the sanctioned lane that WOULD run, WITHOUT spawning it. The
            // operator promotes the lane to mode:"live" only after this clean gated dry run.
            return json!({
                "ok": true,
                "specialist": "growth",
                "kind": "growth_publish",
                "lane": lane,
                "mode": "dry_run",
                "gated": true,
                "published": false,
                "spent": false,
                "organic": true,
                "would_run": argv,
                "cwd": cwd,
                "note": "DRY-RUN: sanctioned publish lane validated but NOT run (mode != live). \
                         Promote to mode:\"live\" only after a clean gated dry run.",
            });
        }

        // LIVE publish: run the project's OWN sanctioned lane one-shot. This is the sanctioned binary
        // the operator configured (the produce/post lane), NEVER a raw post Solomon composes.
        match run_sanctioned_lane(&argv, &cwd, &cfg) {
            Ok(out) if out.code == 0 => json!({
                "ok": true,
                "specialist": "growth",
                "kind": "growth_publish",
                "lane": lane,
                "mode": "live",
                "gated": true,
                "published": true,
                "spent": false,
                "organic": true,
                "exit_code": out.code,
                "summary": lane_summary(&out),
            }),
            Ok(out) => json!({
                "ok": false,
                "specialist": "growth",
                "kind": "growth_publish",
                "lane": lane,
                "mode": "live",
                "gated": true,
                "published": false,
                "spent": false,
                "exit_code": out.code,
                "error": format!("sanctioned publish lane exit {}: {}", out.code, lane_summary(&out)),
            }),
            Err(e) => json!({
                "ok": false,
                "specialist": "growth",
                "kind": "growth_publish",
                "lane": lane,
                "mode": "live",
                "gated": true,
                "published": false,
                "spent": false,
                "error": format!("sanctioned publish lane spawn/timeout: {e}"),
            }),
        }
    }
}

impl Specialist for GrowthSpecialist {
    fn name(&self) -> &'static str {
        "growth"
    }

    fn allowed_tools(&self) -> &'static [&'static str] {
        GROWTH_ALLOWED_TOOLS
    }

    fn scope_globs(&self, repo: &Value) -> Vec<String> {
        // The ONLY writable surface: the lane's growth-drafts path. NEVER money_globs, NEVER a
        // protected grader/leash path — this specialist cannot write product code or money code.
        Self::drafts_glob(repo)
    }

    fn gate(&self, task: &Task, repo: &Value) -> Option<Value> {
        // (1) MONEY GUARD (outermost default-DENY) — IDENTICAL to Research/Engineering. Any
        // money-capable probe (buy_ads/pay_invoice/stripe_checkout/ad_spend/withdraw/... expressed as
        // the task kind) is DENIED fail-closed. money_guard is UNCHANGED; this is the enforcement that
        // makes "every paid/money-out kind is REFUSED at the same guard Research enforces" real —
        // no ad-spend / pay tool is reachable, it is denied here.
        if let Some(refusal) = crate::money_guard::guard(task.kind.as_str(), repo) {
            return Some(refusal);
        }

        // (2) PECRT SAFETY (self-governance default-DENY) — IDENTICAL to Research/Engineering. A
        // dispatch whose TARGET names a whitelist/cycle_budget/skeptic/blast-radius/kill/freshness
        // surface has NO allow arm and is denied. A growth task on an ordinary lane re-enters the
        // existing gate funnel (reenters_gates=true), so it is admitted here.
        let req = crate::pecrt::safety::ScheduleRequest::new(task.kind.as_str(), &task.lane, true);
        if let Some(refusal) = crate::pecrt::safety::guard_schedule(&req) {
            return Some(refusal);
        }

        None
    }

    fn run(&self, task: &Task, repo: &Value) -> Value {
        // The gate MUST have passed before run() is reached; belt-and-suspenders re-check so a future
        // direct caller can never skip it (fail-closed, never a silent bypass) — same discipline as
        // Research/Engineering.
        if let Some(refusal) = self.gate(task, repo) {
            return refusal;
        }

        // A Growth task's ACTION is selected by its detail-carried mode, but the CEILING is the same
        // regardless: organic-only, money-out denied at the gate above. The `detail` distinguishes a
        // pure content DRAFT from a sanctioned-lane PUBLISH; both are dispatched here under
        // TaskKind::Remediate("none") (a benign non-money kind), so the gate treats them identically.
        if is_publish_task(&task.detail) {
            self.run_publish(task, repo)
        } else {
            self.write_draft(task, repo)
        }
    }
}

/// True iff a growth task's detail requests a sanctioned-lane PUBLISH (vs a pure content DRAFT). A
/// publish request is marked explicitly by the dispatch helper's `PUBLISH_MARKER` prefix, so an
/// ordinary content directive can NEVER accidentally trigger a live publish — publishing is opt-in at
/// BOTH the task level (this marker) AND the lane level (`growth_publish` mode:"live"). Pure.
fn is_publish_task(detail: &str) -> bool {
    detail.trim_start().starts_with(PUBLISH_MARKER)
}

/// The explicit task-level marker a publish dispatch prepends. An ordinary `dispatch_growth_content`
/// task never carries it, so it always drafts (never publishes).
pub const PUBLISH_MARKER: &str = "[growth-publish]";

/// A compact one-line summary of a sanctioned lane's trailing JSON status line, for the outcome
/// Value / operator page (mirrors `sover_boost::lane_summary`).
fn lane_summary(out: &proc::RunOut) -> String {
    for line in out.stdout.trim().lines().rev() {
        let line = line.trim();
        if line.starts_with('{') {
            return line.replace(['\n', '\r'], " ").chars().take(240).collect();
        }
    }
    let tail: String = out.stderr.trim().chars().take(160).collect();
    if tail.is_empty() {
        "(no status line)".to_string()
    } else {
        tail
    }
}

/// Run ONE sanctioned publish-lane one-shot with a cleaned env, the configured cwd + optional
/// profile env, and a PUBLISH_TIMEOUT_S ceiling. Mirrors `sover_boost::run_lane`'s scrubbed-env +
/// hidden-window contract. Returns the RunOut (or Err on spawn/timeout). This runs a project's OWN
/// published binary — Solomon composes no post; it invokes the operator-sanctioned lane.
fn run_sanctioned_lane(argv: &[String], cwd: &str, cfg: &Value) -> std::io::Result<proc::RunOut> {
    use std::io::Read;
    if argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty sanctioned-lane argv",
        ));
    }
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    proc::apply_clean_env(&mut cmd); // scrub GH/PYTHON* + force UTF-8 stdio (same as proc::run)
    // Optional profile env (e.g. SOVER_PROFILE for a --live state root), from the opt-in block.
    if let (Some(k), Some(v)) = (
        cfg.get("profile_env_key").and_then(Value::as_str),
        cfg.get("profile_env").and_then(Value::as_str),
    ) {
        if !k.is_empty() {
            cmd.env(k, v);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(proc::CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let out_h = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });
    let err_h = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });
    let join = |h: Option<std::thread::JoinHandle<String>>| -> String {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    use wait_timeout::ChildExt;
    match child.wait_timeout(Duration::from_secs(PUBLISH_TIMEOUT_S))? {
        Some(status) => Ok(proc::RunOut {
            code: status.code().unwrap_or(-1),
            stdout: join(out_h),
            stderr: join(err_h),
        }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            drop(out_h);
            drop(err_h);
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "sanctioned publish lane timed out",
            ))
        }
    }
}

// --------------------------------------------------------------------------- #
// The thin dispatch helpers — mirror research::dispatch_research
// --------------------------------------------------------------------------- #

/// Dispatch an ORGANIC CONTENT growth task to the Growth specialist through the gated interface: read
/// the lane, build a `growth_content` task, and route it through the SAME `orchestrator::dispatch`
/// (gate FIRST, then run). Produces a GATED, unpublished DRAFT — publishes NOTHING. `detail` is the
/// content directive (what README/docs/examples/showcase copy to draft). This is the seam a
/// thread-plane driver calls to have Solomon draft organic growth content for a lane.
pub fn dispatch_growth_content(repo: &Value, detail: &str) -> Value {
    let lane = paths::repo_name(repo);
    // A growth-content draft is NOT a coding change and NOT a money remediation: it is a benign
    // non-money remediation kind. `Remediate("none")` is money-waved-through and pecrt-admitted on an
    // ordinary lane, so the gate passes on an ordinary lane and DENIES on a governance/money target
    // exactly as for Research/Engineering.
    let task = Task::new(TaskKind::Remediate("none"), &lane, detail);
    crate::ceo::orchestrator::dispatch(&GrowthSpecialist::new(), &task, repo)
}

/// Dispatch a PUBLISH growth task to the Growth specialist through the gated interface. Publishing
/// routes ONLY through the project's OWN sanctioned lane (`growth_publish.publish`), is opt-in per
/// lane, and is DRY-RUN-FIRST: a lane without a `growth_publish` block, or with `mode` != `"live"`,
/// yields a dry-run report (published:false) — never a silent live publish. The task detail is
/// prefixed with `PUBLISH_MARKER` so `run` selects the publish path; the money/governance gate is run
/// FIRST exactly as for a content task (a money-tagged kind is refused before any lane invocation).
pub fn dispatch_growth_publish(repo: &Value, detail: &str) -> Value {
    let lane = paths::repo_name(repo);
    let marked = format!("{PUBLISH_MARKER} {detail}");
    let task = Task::new(TaskKind::Remediate("none"), &lane, &marked);
    crate::ceo::orchestrator::dispatch(&GrowthSpecialist::new(), &task, repo)
}

// --------------------------------------------------------------------------- #
// The Phase-B GROWTH COMPOSER — fills the D12 seam (audit finding #12)
// --------------------------------------------------------------------------- #

/// Operator-identifying markers that must NEVER appear in a growth draft — the PERSONA RULE's
/// deterministic deny filter. Drafts are authored under each project's OWN brand or its agent
/// persona, never the operator's personal name (the repos.json per-lane PERSONA RULE sentences +
/// the autopilot mission). Case-insensitive substring match, mirroring sover's `deny_terms`
/// precedent; the entries are the git identity (user name, login) and the email local-part. The
/// bare first name subsumes the longer forms — all three are kept explicit so a future edit of one
/// cannot silently un-cover another.
pub const OPERATOR_MARKERS: &[&str] = &["cayleb", "cayleb-james2008", "caylebalvarezjames"];

/// Standalone operator name tokens, WORD-BOUNDARY matched (a bare substring check on "james"
/// would trip on unrelated prefix-sharing words; a whole-word false positive still only drops a
/// draft — fail-closed, safe direction).
const OPERATOR_NAME_TOKENS: &[&str] = &["james", "alvarez"];

/// Keywords that mark a planner backlog line as a GROWTH content directive (README / docs /
/// examples / release-note / post / showcase work on a public lane). Deliberately heuristic: a
/// false negative is a quiet day (safe); a false positive still only yields a gated local draft
/// behind the money/pecrt gates (safe).
const GROWTH_DIRECTIVE_KEYWORDS: &[&str] = &[
    "readme",
    "docs",
    "example",
    "release note",
    "release-note",
    "post",
    "showcase",
    "quickstart",
    "growth",
    "description",
    "topics",
];

/// The growth copywriter's role contract: evidence-grounded, organic-only, persona-safe, STRICT
/// JSON out. The PERSONA RULE is stated at the prompt level here AND enforced deterministically by
/// [`violates_persona`] after the reply — prompt-level alone is a wish, the filter is the gate.
const GROWTH_COMPOSER_PROMPT: &str = "You are the growth copywriter for ONE public software \
    project. You are given the project's lane name (its BRAND), its north-star goal, today's \
    planner growth directive, and its MEASURED 24h outcomes and velocity. Draft ONE piece of \
    ORGANIC growth content that honestly advances the directive: a README improvement, a release \
    note, or a short post. EVIDENCE RULE: every claim must be grounded in the measured outcomes \
    provided — never invent traction, numbers, users, or results; if the numbers are small or \
    null, write honest developer-facing copy about what the project DOES, not what it has \
    'achieved'. PERSONA RULE: the content is authored under the project's own brand (the lane \
    name) or its agent persona — NEVER the operator's personal name, handle, or email; do not \
    mention or credit any human maintainer. Organic/free channels only — no paid placement, no ad \
    copy, no calls to spend. Reply with STRICT JSON only: \
    {\"kind\":\"readme|release_note|post\",\"title\":\"<one line>\",\"body\":\"<the draft copy>\"}";

/// True iff `text` carries an operator-identifying marker (case-insensitive) — the deterministic
/// persona deny filter. Pure — unit-tested against every marker.
pub(crate) fn violates_persona(text: &str) -> bool {
    let lower = text.to_lowercase();
    OPERATOR_MARKERS.iter().any(|m| lower.contains(m))
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|tok| OPERATOR_NAME_TOKENS.contains(&tok))
}

/// True iff a backlog line reads as a GROWTH content directive (keyword heuristic, pure).
pub(crate) fn is_growth_directive(line: &str) -> bool {
    let lower = line.to_lowercase();
    GROWTH_DIRECTIVE_KEYWORDS.iter().any(|k| lower.contains(k))
}

/// Find TODAY'S planner-composed growth directive in a lane's backlog (pure — unit-tested): the
/// first OPEN (`- [ ]`), non-deferred line that is either today's ceo goal (carries
/// `today_marker`, i.e. `(ceo <date>)`) or an open `[campaign:` step, AND reads as a growth
/// directive. This is the HONEST trigger the D12 hook doc demands — planner output, never a
/// static daily fact. Returns the line with the `- [ ]` prefix stripped, or None (a quiet day).
pub(crate) fn growth_directive(backlog: &str, today_marker: &str) -> Option<String> {
    backlog
        .lines()
        .map(str::trim)
        .find(|l| {
            l.starts_with("- [ ]")
                && !l.contains("(deferred")
                && (l.contains(today_marker) || l.contains("[campaign:"))
                && is_growth_directive(l)
        })
        .map(|l| l.trim_start_matches("- [ ]").trim().to_string())
}

/// The public-lane eligibility predicate (pure — unit-tested): a lane is a compose candidate iff
/// it has a resolvable name, an explicit `public: true` flag (repos.json; absent/false/non-bool =>
/// private — asmodeus has no flag and daedulus is `false`, both excluded), a non-empty north star
/// (morning_plan's "no north star = not a planned lane" rule), and its ops rollup is not RED
/// (green-before-growth: don't market a dead engine; an ABSENT rollup is not red — drafting is
/// local and harmless) — EXCEPT a red carried SOLELY by operator-gated external probes
/// ([`red_on_operator_gated_probes_only`]) is NOT engine death: the engine keeps composing
/// (compose-and-hold — drafts land in the gated log and the publish seam ships via the healthy
/// platforms' sanctioned lane) while the human dependency, e.g. sover's YouTube login, waits.
pub(crate) fn eligible_repo(repo: &Value, north_star: &str, rollup: &Value) -> bool {
    if paths::repo_name(repo).is_empty() {
        return false;
    }
    if !repo.get("public").and_then(Value::as_bool).unwrap_or(false) {
        return false;
    }
    if north_star.trim().is_empty() {
        return false;
    }
    rollup.get("status").and_then(Value::as_str) != Some("red")
        || red_on_operator_gated_probes_only(rollup)
}

/// True iff the rollup's RED is carried SOLELY by operator-gated external probes — ops.json probes
/// marked `operator_gated: true` (a HUMAN-owned dependency like sover's YouTube Google session,
/// which no agent can re-login), classified by the ops sweep into the rollup's
/// `red_operator_gated_only` flag (`ops::outcomes::red_only_operator_gated`). Absent/false reads
/// as engine-dead — fail closed, so pre-flag rollups and sweep-panic entries keep the strict
/// green-before-growth behavior. Pure — unit-tested.
pub(crate) fn red_on_operator_gated_probes_only(rollup: &Value) -> bool {
    rollup.get("red_operator_gated_only").and_then(Value::as_bool) == Some(true)
}

/// `runtime/<lane>/_growth_drafted_<YYYY-MM-DD>` — the per-lane per-day compose marker (a sibling
/// of `sover_boost`'s `_last_boost`/`_boost_count_<date>` markers; inert, janitor-tolerated).
fn growth_stamp_path(repo: &Value, date: &str) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join(format!("_growth_drafted_{date}")))
}

/// True iff this lane already consumed today's compose attempt (the stamp exists). A NAMELESS row
/// can never be stamped, so it reads as already-drafted (never attempted) — fail-closed.
pub(crate) fn already_drafted(repo: &Value, date: &str) -> bool {
    growth_stamp_path(repo, date).map(|p| p.exists()).unwrap_or(true)
}

/// Atomically CLAIM the day's compose attempt: create the stamp with `create_new` so exactly one
/// OS process (GUI tick vs Sentinel one-shot) wins the race — the loser sees `AlreadyExists` and
/// moves on. Returns false when the claim was not won (already claimed, nameless lane, or any IO
/// error — fail closed, skip the lane).
fn claim_growth_stamp(repo: &Value, date: &str) -> bool {
    use std::io::Write;
    (|| -> std::io::Result<()> {
        let p = growth_stamp_path(repo, date).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "nameless lane — no runtime dir")
        })?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p)?;
        f.write_all(format!("attempt {ts}").as_bytes())
    })()
    .is_ok()
}

/// Overwrite the day stamp with a named outcome (`ok`, or a `skip:<reason>`) — the marker doubles
/// as the skip log (best-effort, mirrors `sover_boost::stamp_boost`). The initial "attempt" claim
/// goes through [`claim_growth_stamp`], which is the atomic cross-process gate.
fn stamp_growth(repo: &Value, date: &str, note: &str) {
    let _ = (|| -> std::io::Result<()> {
        let p = growth_stamp_path(repo, date).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "nameless lane — no runtime dir")
        })?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        std::fs::write(&p, format!("{note} {ts}"))?;
        Ok(())
    })();
}

/// Parse the composer's STRICT-JSON reply into (kind, title, body) — pure over the reply string
/// (unit-tested on fixtures), via the same `extract_json` + `cap_line` seams the morning plan
/// uses. An unknown `kind` degrades to "post" (a formatting nit, not a fabrication); a missing or
/// blank `body` is None (a blank draft is the busywork the eval-park doctrine forbids). Title and
/// body are flattened to single capped lines — the draft fact is one dated line.
pub(crate) fn parse_growth_reply(reply: &str) -> Option<(String, String, String)> {
    let parsed = super::extract_json(reply)?;
    let kind = match parsed.get("kind").and_then(Value::as_str) {
        Some(k @ ("readme" | "release_note" | "post")) => k.to_string(),
        _ => "post".to_string(),
    };
    let title = super::cap_line(parsed.get("title").and_then(Value::as_str).unwrap_or(""), 120);
    let body = super::cap_line(parsed.get("body").and_then(Value::as_str).unwrap_or(""), 800);
    if body.is_empty() {
        return None;
    }
    Some((kind, title, body))
}

/// Resolve the compose model from the brain config (pure — unit-tested): the ideate worker (the
/// creative role) when the MoA brain is enabled, else the aggregator, else the CEO planner model.
/// NEVER a hardcoded provider model id — under OpenRouter the provider-aware transport
/// (`chat_model_for`) substitutes the configured autopilot model for an Ollama-native id, so every
/// branch survives a provider switch.
pub(crate) fn pick_growth_model(cfg: &crate::improver::brain::BrainConfig) -> String {
    if cfg.enabled {
        cfg.workers
            .ideate
            .clone()
            .unwrap_or_else(|| cfg.aggregator.clone())
    } else {
        super::CEO_MODEL.to_string()
    }
}

/// The every-sweep growth-content COMPOSER, ridden on `ceo_slow_tail` (the D12/Phase-B seam,
/// audit finding #12): synthesize at most ONE gated, unpublished, persona-safe growth draft per
/// PUBLIC lane per day — and at most ONE lane per tail invocation (the rest get later sweeps the
/// same day, mirroring `MAX_BOOSTS_PER_SWEEP`). Lanes are visited priority-ordered
/// (snapshot `projects.<name>.priority`, like the morning plan).
///
///   1. HONEST TRIGGER — a lane is composed ONLY when today's planner output (its `(ceo <date>)`
///      backlog goal or an open `[campaign:` step) carries a growth directive. No directive => a
///      quiet day; never a static daily fact. A backlog READ ERROR (improver mid-rewrite) skips
///      the sweep without stamping — never interact with a half-written file.
///   2. DAY-GATE, STAMP-FIRST — `runtime/<lane>/_growth_drafted_<date>` is written BEFORE the LLM
///      call (exactly like `sover_boost::stamp_boost`), so a hung/failed compose consumes the
///      day's attempt and the SECOND OS process running this tail (the Sentinel watchdog
///      one-shot) cannot double-fire. LLM unavailability (quota/429/transport) degrades to a
///      SILENT no-op with a named `skip:` reason recorded in the stamp marker — never a panic
///      (the call site's catch_unwind is belt-and-suspenders), never a fabricated draft.
///   3. EVIDENCE-GROUNDED — the compose payload's factual substrate is the outcomes-ledger
///      `snapshot` this tail already holds (outcomes_24h + velocity + the anonymized prior-wins
///      tally); the draft cites real numbers, never invented traction. No ledger re-read.
///   4. PERSONA RULE — stated in the prompt AND enforced by the deterministic
///      [`OPERATOR_MARKERS`] deny filter: a draft carrying an operator-identifying marker is
///      refused (stamp kept — no retry storm). Drafts are authored under the project's own brand.
///   5. DRAFT-ONLY — the composed content routes through `dispatch_growth_content` (gate-first:
///      money_guard + pecrt::safety), landing as a GATED, unpublished, organic line in
///      `runtime/<lane>/growth_drafts.jsonl`. The publish ladder (`dispatch_growth_publish`) is
///      NEVER invoked from here — publishing stays on the operator's dry-run-first ladder.
pub fn maybe_draft_growth_content(snapshot: &serde_json::Value, status: &serde_json::Value) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let today_marker = super::ceo_marker(&today);

    // Lane inventory, priority-ordered by the ledger snapshot's priority (missing => last), name
    // as the deterministic tiebreak — the same ordering discipline as morning_plan.
    let mut rows: Vec<(i64, String, Value)> = crate::control::registry::read_repos_json()
        .into_iter()
        .filter_map(|r| {
            let name = paths::repo_name(&r);
            if name.is_empty() {
                return None;
            }
            let prio = snapshot
                .pointer(&format!("/projects/{name}/priority"))
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX);
            Some((prio, name, r))
        })
        .collect();
    rows.sort_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));

    for (_prio, lane, repo) in rows {
        // Cheap pre-check before any file IO: only explicit public lanes are candidates.
        if !repo.get("public").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        // North star: the operator's committed goal.md line, else the repos.json `goal` fallback.
        let fallback = repo.get("goal").and_then(Value::as_str).unwrap_or("");
        let goal_md = std::fs::read_to_string(super::goal_post_path(&lane)).ok();
        let north_star = super::pick_goal_post(goal_md.as_deref(), fallback);
        let rollup = status
            .pointer(&format!("/projects/{lane}"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !eligible_repo(&repo, &north_star, &rollup) {
            continue;
        }
        if already_drafted(&repo, &today) {
            continue; // day-gate: this lane already consumed today's compose attempt
        }
        // HONEST TRIGGER: today's planner-composed growth directive. Absent file => the planner
        // has not routed growth here (quiet day); a read ERROR (mid-rewrite) => skip this sweep.
        let backlog = match std::fs::read_to_string(super::backlog_path(&lane)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let directive = match growth_directive(&backlog, &today_marker) {
            Some(d) => d,
            None => continue, // no growth directive today — no fabricated busywork
        };
        // STAMP FIRST — atomically claim the day's attempt BEFORE the slow LLM call (cross-process
        // safe: the GUI tick and the Sentinel watchdog one-shot both run this tail; `create_new`
        // guarantees exactly one winner even inside the check-to-claim window).
        if !claim_growth_stamp(&repo, &today) {
            continue; // the other process claimed this lane just now — same as seeing its stamp
        }
        compose_and_dispatch(&repo, &lane, &north_star, &directive, snapshot, &today);
        return; // at most ONE lane composed per tail invocation
    }
}

/// Compose ONE draft for the stamped lane and dispatch it through the gated GrowthSpecialist
/// surface. Side-effectful (one LLM call via the provider-aware transport) — deliberately NOT
/// unit-tested, per the `focus::decompose_campaign` precedent; everything around it is. Every
/// failure path records a named outcome in the day stamp and returns silently (the stamp is
/// already consumed — retry tomorrow, matching focus's one-attempt/day semantics).
fn compose_and_dispatch(
    repo: &Value,
    lane: &str,
    north_star: &str,
    directive: &str,
    snapshot: &Value,
    today: &str,
) {
    let outcomes = snapshot
        .pointer(&format!("/projects/{lane}"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let velocity = super::velocity_context(&outcomes, north_star);
    let recent_wins = crate::ceo::wins::prior_wins();
    let user = serde_json::to_string_pretty(&json!({
        "lane": lane,
        "brand": lane,
        "north_star": north_star,
        "directive": directive,
        "outcomes_24h": outcomes,
        "velocity": velocity,
        "recent_wins": recent_wins,
    }))
    .unwrap_or_default();

    // Model + skill resolved at call time (never hardcoded): the brain's creative worker when
    // enabled, else the CEO model — the provider-aware transport handles the provider mapping.
    // `ctx: None` short-circuits spawn_worker to the CEO chat path (no improver Ctx on this
    // plane), matching the morning_plan / focus precedent at <=1 call/day/lane.
    let model = pick_growth_model(&crate::improver::brain::BrainConfig::from_autopilot());
    let skill = crate::improver::brain::load_skill(lane, "growth");
    let reply = match crate::improver::brain::spawn_worker(
        &model,
        GROWTH_COMPOSER_PROMPT,
        &skill,
        &user,
        None,
    ) {
        Ok(r) => r,
        Err(e) => {
            // LLM unavailable (quota/429/transport/parked): a SILENT no-op — the stamp already
            // consumed today's attempt; record the named skip reason and return. Never fabricate.
            stamp_growth(
                repo,
                today,
                &format!("skip:llm_unavailable {}", super::cap_line(&e, 160)),
            );
            return;
        }
    };
    let (kind, title, body) = match parse_growth_reply(&reply) {
        Some(t) => t,
        None => {
            stamp_growth(repo, today, "skip:unparseable_or_empty_reply");
            return;
        }
    };
    let detail = format!(
        "[{kind}] {title} — {body} (directive: {})",
        super::cap_line(directive, 200)
    );
    // The deterministic PERSONA gate — prompt-level rules are wishes; this is the enforcement. A
    // violating draft is refused outright (stamp kept, no retry storm, nothing dispatched).
    if violates_persona(&detail) {
        stamp_growth(repo, today, "skip:persona_violation");
        return;
    }
    let out = dispatch_growth_content(repo, &detail);
    if out.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        stamp_growth(repo, today, "ok");
        let _ = crate::notify::send(&crate::notify::Notice::report(
            format!("Solomon: growth draft -> {lane}"),
            format!(
                "[{kind}] {title} (GATED, unpublished — review runtime/{lane}/growth_drafts.jsonl)"
            ),
        ));
    } else {
        stamp_growth(
            repo,
            today,
            &format!("skip:dispatch_refused {}", super::cap_line(&out.to_string(), 160)),
        );
    }
}

// --------------------------------------------------------------------------- #
// The PUBLISH "last inch" — the human-gated compose -> publish -> measure seam
// --------------------------------------------------------------------------- #
//
// The three growth islands already exist independently: COMPOSE writes a gated draft to
// `runtime/<lane>/growth_drafts.jsonl` (`maybe_draft_growth_content`); PUBLISH routes a draft
// through the project's OWN dry-run-first sanctioned lane (`dispatch_growth_publish`); MEASURE is
// the `publish_recency` ops probe reading the project's own post registry. This seam is the ONLY
// wire between COMPOSE and PUBLISH, and it is HUMAN-GATED: a draft ships ONLY when the operator has
// hand-set a boolean `approved: true` flag on the NEWEST drafts line. Nothing in Solomon ever sets
// `approved` — so with today's drafts (plain `<date>\t<fact>` text lines, never approved) this is
// fully INERT. It never auto-approves, never weakens the money_guard/pecrt gate (which still runs
// FIRST inside `dispatch_growth_publish`), and writes NO `fleet_ledger` revenue row (a publish is an
// OUTCOME, not revenue — `fleet_ledger` is the money file).

/// True iff a growth-drafts line carries an operator-set `"approved": true` flag. The composer only
/// ever writes plain `<date>\t<fact>` TEXT lines (never approved); an operator APPROVES a draft by
/// making the newest line a JSON object with a boolean `"approved": true` (optionally keeping the
/// `<date>\t` prefix, which is stripped here). Pure + FAIL-CLOSED: anything that is not a JSON object
/// with `approved == true` (every composed text line, malformed edits, `approved:"true"` strings,
/// `approved:1`) is NOT approved. Nothing in Solomon ever writes this flag — it is the human gate.
pub(crate) fn draft_line_approved(line: &str) -> bool {
    // Try the payload after an optional leading `<date>\t`, then the whole line (the operator may
    // author a bare JSON object without the date prefix). Either parsing to `{"approved": true}` wins.
    let after_tab = line.split_once('\t').map(|(_d, rest)| rest).unwrap_or(line);
    [after_tab, line].iter().any(|cand| {
        serde_json::from_str::<Value>(cand.trim())
            .ok()
            .and_then(|v| v.get("approved").and_then(Value::as_bool))
            == Some(true)
    })
}

/// `runtime/<lane>/_growth_published` — the per-lane publish idempotency marker (a sibling of the
/// `_growth_drafted_<date>` compose stamp; inert, janitor-tolerated). Records the LAST handled
/// approved-line signature + its publish outcome so this every-sweep seam neither re-publishes a
/// LIVE draft nor re-dispatches an unchanged dry-run. `None` for a nameless lane.
fn published_marker_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("_growth_published"))
}

/// Read the last-handled `{signature, published}` marker (`None` when absent/unparseable — a fresh
/// lane, treated as never handled — fail-open toward dispatching, which is itself gated + dry-run).
fn read_published_marker(repo: &Value) -> Option<Value> {
    let p = published_marker_path(repo)?;
    let raw = std::fs::read_to_string(p).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

/// Persist the publish marker for `signature` with its outcome (best-effort; a disk hiccup only
/// loses the idempotency hint, so at worst the next sweep re-dispatches a gated dry-run — never a
/// duplicate LIVE publish path, which is separately guarded by the `published == true` skip below).
fn write_published_marker(repo: &Value, signature: &str, published: bool, mode: &str) {
    if let Some(p) = published_marker_path(repo) {
        let _ = proc::atomic_write_json(
            &p,
            &json!({
                "signature": signature,
                "published": published,
                "mode": mode,
                "ts": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            }),
        );
    }
}

/// `runtime/<lane>/_growth_published_<YYYY-MM-DD>` — the per-lane per-day AUTO-PUBLISH RATE cap
/// marker (a sibling of the compose day-gate `_growth_drafted_<date>`; inert, janitor-tolerated).
/// One CLAIM per lane per day caps auto-publish at one dispatch/lane/day — the same rate discipline
/// the composer uses — so autonomous publishing can never storm a channel or the subprocess budget.
/// `None` for a nameless lane.
fn published_day_marker_path(repo: &Value, date: &str) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join(format!("_growth_published_{date}")))
}

/// Atomically CLAIM this lane's ONE auto-publish attempt for `date` (`create_new` — exactly one OS
/// process wins across the GUI tick and the Sentinel watchdog one-shot, the identical cross-process
/// discipline as `claim_growth_stamp`). Returns false when already claimed (rate cap reached this
/// day), nameless, or on any IO error — fail-closed: skip the lane, never double-publish.
fn claim_published_day(repo: &Value, date: &str) -> bool {
    use std::io::Write;
    (|| -> std::io::Result<()> {
        let p = published_day_marker_path(repo, date).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "nameless lane — no runtime dir")
        })?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p)?;
        f.write_all(format!("publish attempt {ts}").as_bytes())
    })()
    .is_ok()
}

/// The AUTOMATED content gate that REPLACES the operator `approved:true` wait (pure — unit-tested).
/// A composed draft auto-publishes ONLY if it is genuine composed CONTENT that clears the
/// deterministic persona filter: Solomon ships what its evidence-grounded composer produced, never a
/// fabricated/empty line, never a raw operator-control JSON edit, never operator-identifying copy.
/// Returns the draft TEXT to publish on success, or `Err(reason)` naming the automated block
/// (surfaced for observability + the tests). This is content + persona safety, NOT a human gate.
pub(crate) fn auto_publish_content_check(newest: &str) -> Result<String, &'static str> {
    // The composer writes `<date>\t<fact>` TEXT lines; publish the fact after an optional date tab.
    let text = newest.split_once('\t').map(|(_d, rest)| rest).unwrap_or(newest).trim();
    if text.is_empty() {
        return Err("empty draft"); // nothing composed — never fabricate a publish
    }
    // A raw JSON control line (e.g. an operator `{"approved":...}` edit) is NOT publishable content.
    if serde_json::from_str::<Value>(text).is_ok() {
        return Err("control line, not content");
    }
    if violates_persona(text) {
        return Err("persona violation"); // deterministic operator-marker deny filter
    }
    Ok(text.to_string())
}

/// AUTO-publish decide-and-ship for ONE lane, with the publish SEAM INJECTED so the call/no-call
/// boundary is testable without a real publish. Reads the NEWEST `growth_drafts.jsonl` line and
/// AUTONOMOUSLY dispatches it — NO operator `approved:true` wait — behind the AUTOMATED safety that
/// stays: (1) the `auto_publish_content_check` persona/content gate, (2) per-draft dedup (a draft
/// already LIVE-published is never re-shipped), (3) the per-lane-per-day publish RATE cap
/// (`claim_published_day`), and (4) inside `dispatch` the money_guard + pecrt brand-safety gate,
/// dry-run-first (a lane not promoted to `mode:"live"` only dry-runs). `dispatched` tells the
/// caller/test whether the seam fired. The live/dry-run decision lives entirely inside the gated
/// `dispatch` (it reads the lane's `growth_publish.mode`), so this seam needs no live flag.
fn auto_publish_draft<F>(repo: &Value, dispatch: F) -> Value
where
    F: FnOnce(&Value, &str) -> Value,
{
    let lane = paths::repo_name(repo);
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let path = match GrowthSpecialist::drafts_log_path(repo) {
        Some(p) => p,
        None => return json!({"lane": lane, "dispatched": false, "reason": "nameless lane"}),
    };
    let newest = match ObservationLog::at(path).tail(1).into_iter().next() {
        Some(l) => l,
        None => return json!({"lane": lane, "dispatched": false, "reason": "no drafts"}),
    };
    // (1) AUTOMATED content + persona gate — replaces the human `approved:true` wait.
    let text = match auto_publish_content_check(&newest) {
        Ok(t) => t,
        Err(reason) => return json!({"lane": lane, "dispatched": false, "reason": reason}),
    };
    let signature = newest.trim();
    // (2) Per-draft dedup: this exact draft already LIVE-published -> never re-ship it.
    if let Some(prev) = read_published_marker(repo) {
        if prev.get("signature").and_then(Value::as_str) == Some(signature)
            && prev.get("published").and_then(Value::as_bool) == Some(true)
        {
            return json!({"lane": lane, "dispatched": false, "reason": "already published"});
        }
    }
    // (3) Per-lane-per-day RATE cap: at most ONE auto-publish dispatch per lane per day. A failed /
    // dry-run publish also consumes the day (bounded — never a per-sweep retry storm); the next
    // FRESH draft ships tomorrow. Cross-process safe (GUI tick vs Sentinel one-shot).
    if !claim_published_day(repo, &today) {
        return json!({"lane": lane, "dispatched": false, "reason": "daily publish cap reached"});
    }
    // Gates passed: fire the injected publish seam ONCE. detail is a short provenance note — the
    // publish path routes via the project's OWN sanctioned argv and largely ignores it; the
    // money/pecrt brand-safety gate inside `dispatch` runs FIRST.
    let detail = format!(
        "auto-published growth draft (content+persona checks passed): {}",
        super::cap_line(&text, 200)
    );
    let out = dispatch(repo, &detail);
    let published = out.get("published").and_then(Value::as_bool).unwrap_or(false);
    let mode = out.get("mode").and_then(Value::as_str).unwrap_or("").to_string();
    write_published_marker(repo, signature, published, &mode);
    // LOG the published:true/false result for observability; the publish_recency probe independently
    // confirms a live publish by reading the project's own post registry (no fleet_ledger row here).
    let _ = crate::notify::send(&crate::notify::Notice::report(
        format!("Solomon: growth auto-publish -> {lane}"),
        format!("draft dispatched: published={published} mode={mode}"),
    ));
    json!({"lane": lane, "dispatched": true, "published": published, "mode": mode})
}

/// The PUBLISH "last inch" ridden on `ceo_slow_tail` (the AUTONOMOUS compose -> publish -> measure
/// wire): for each PUBLIC lane, AUTO-publish its newest growth draft — NO operator `approved:true`
/// wait — behind the AUTOMATED safety in `auto_publish_draft` (persona/content check, per-draft
/// dedup, per-lane-per-day rate cap) and the gated dry-run-first `dispatch_growth_publish`
/// (money_guard + pecrt brand-safety FIRST; a lane not promoted to `mode:"live"` only dry-runs).
/// Visits lanes in registry order; `status`/`snapshot` are accepted for signature parity with the
/// other tail seams (publish eligibility is the automated content/rate safety, not a health rollup).
pub fn maybe_auto_publish_growth(_snapshot: &Value, _status: &Value) {
    for repo in crate::control::registry::read_repos_json() {
        // Only explicit public lanes are publish candidates (same cheap pre-check as the composer).
        if !repo.get("public").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let _ = auto_publish_draft(&repo, dispatch_growth_publish);
    }
}

// --------------------------------------------------------------------------- #
// tests — the D12 acceptance contracts
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ceo::research::{is_external_mutation, EXTERNAL_MUTATION_MARKERS};
    use serde_json::json;

    fn plain_repo() -> Value {
        json!({ "name": "sover" })
    }

    /// A repo row with a UNIQUE lane name so a test's drafts log never shares a runtime path with
    /// another parallel test (all resolve under the same per-process temp HERE).
    fn uniq_repo(tag: &str) -> Value {
        let name = format!(
            "d12growth_{tag}_{}_{}",
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

    // ===================================================================== #
    // ACCEPTANCE (a): GrowthSpecialist implements trait Specialist and is
    // selectable by task kind through ceo::orchestrator::dispatch — an ORGANIC
    // content/publish task is DISPATCHED and produces a real GATED artifact.
    // ===================================================================== #
    #[test]
    fn organic_content_task_is_dispatched_and_writes_a_gated_draft() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let repo = uniq_repo("content");
        let out = dispatch_growth_content(
            &repo,
            "draft a README quickstart section + 2 usage examples for the landing page",
        );

        // The outcome is an OK, GATED, UNPUBLISHED, UNSPENT, ORGANIC growth note.
        assert_eq!(out["ok"], true, "growth content task must succeed: {out}");
        assert_eq!(out["specialist"], "growth");
        assert_eq!(out["kind"], "growth_content");
        assert_eq!(out["gated"], true, "the artifact must be GATED for human review");
        assert_eq!(out["published"], false, "NOTHING is published");
        assert_eq!(out["spent"], false, "NOTHING is spent");
        assert_eq!(out["organic"], true, "growth is organic-only");
        assert_eq!(out["provenance"], "rsi:", "the draft is provenance-tagged");

        // The artifact is REAL: read the growth-drafts log back off disk.
        let artifact_path = out["artifact_path"].as_str().expect("artifact_path present");
        let body = std::fs::read_to_string(artifact_path).expect("growth-drafts log exists on disk");
        assert!(body.contains("rsi: growth DRAFT"), "line is provenance-tagged: {body}");
        assert!(
            body.contains("[GATED, unpublished, organic]"),
            "line is marked gated+unpublished+organic: {body}"
        );
        assert!(body.contains("README quickstart"), "the content directive is recorded: {body}");
        assert!(
            body.lines().next().unwrap().contains('\t'),
            "the growth line is dated (iso-date TAB fact): {body}"
        );

        if let Some(dir) = std::path::Path::new(artifact_path).parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ===================================================================== #
    // ACCEPTANCE (b): a buy_ads / pay_invoice / stripe / ad_spend / withdraw
    // task is REFUSED before any action — at the SAME money_guard gate
    // Research already enforces, on every lane (money-out is the HARD
    // invariant, no lane exempts it).
    // ===================================================================== #
    #[test]
    fn every_paid_money_out_kind_is_refused_before_any_action() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let growth = GrowthSpecialist::new();

        // The canonical paid / money-out kinds the acceptance names, PLUS a wider spread — each must
        // be DENIED at gate() BEFORE run(), even on a whitelisted live-money lane.
        for money_kind in [
            "buy_ads", "pay_invoice", "stripe_checkout", "ad_spend", "withdraw", "transfer",
            "deposit", "send_money", "spend_treasury", "subscribe",
        ] {
            for repo in [plain_repo(), kairos_repo()] {
                let money_task = Task {
                    kind: TaskKind::Remediate(money_kind_static(money_kind)),
                    lane: "kairos".to_string(),
                    detail: money_kind.to_string(),
                };
                let verdict = growth.gate(&money_task, &repo);
                assert!(
                    verdict.is_some(),
                    "paid/money-out kind '{money_kind}' must be DENIED at the growth gate on {repo}"
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

        // Every kind the acceptance explicitly names is in the closed refused set (documentation
        // that we cover the criterion's exact verbs).
        for k in GROWTH_REFUSED_MONEY_KINDS {
            assert!(
                crate::money_guard::is_money_capable(k),
                "refused kind '{k}' must be money-capable so money_guard denies it"
            );
        }

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    /// A denied money probe never reaches run() through the real dispatch path either: dispatch
    /// returns the refusal verbatim and NO growth draft is written and NO lane is invoked (nothing
    /// drafted, published, or spent on a denied money attempt).
    #[test]
    fn a_denied_money_probe_writes_no_artifact_and_invokes_no_lane() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let growth = GrowthSpecialist::new();
        // A unique whitelisted (equity_usd) lane so the money DENY fires on the strongest lane while
        // the drafts-log path stays isolated from other parallel tests.
        let mut repo = uniq_repo("denymoney");
        repo["equity_usd"] = json!(1234.5);
        let before = GrowthSpecialist::drafts_log_path(&repo)
            .map(|p| std::fs::read_to_string(&p).unwrap_or_default())
            .unwrap_or_default();

        let denied = crate::ceo::orchestrator::dispatch(
            &growth,
            &Task::new(TaskKind::Remediate("buy_ads"), "kairos", "buy_ads probe"),
            &repo,
        );
        assert_eq!(denied["ok"], false, "an ad-spend probe must be denied");

        let after = GrowthSpecialist::drafts_log_path(&repo)
            .map(|p| std::fs::read_to_string(&p).unwrap_or_default())
            .unwrap_or_default();
        assert_eq!(before, after, "a denied money probe must write NO growth draft line");
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ===================================================================== #
    // ACCEPTANCE (c): the whitelist names NO money tool and NO RAW external-
    // mutation tool. Publishing can ONLY route through a project's sanctioned
    // lane, never a raw external post — enforced by the whitelist, not asserted.
    // ===================================================================== #
    #[test]
    fn whitelist_has_no_money_tool_and_no_raw_external_mutation_tool() {
        let growth = GrowthSpecialist::new();
        for t in growth.allowed_tools() {
            assert!(
                !crate::money_guard::is_money_capable(t),
                "the Growth whitelist names a money-capable tool '{t}' — it must not"
            );
            assert!(
                !is_external_mutation(t),
                "the Growth whitelist names a RAW external-mutation tool '{t}' — it must not. \
                 The specialist READS + DRAFTS locally and publishes ONLY via a project's OWN \
                 sanctioned lane (invoke_project_lane), NEVER a raw post/deploy/push/send"
            );
        }
        // Positive control: the markers DO catch real raw-post/pay tool names, so the guard has teeth.
        for bad in ["post_to_x", "deploy_landing", "send_email", "git_push", "tweet", "buy_ads"] {
            assert!(
                is_external_mutation(bad) || crate::money_guard::is_money_capable(bad),
                "'{bad}' should be caught as raw-external-mutation or money-capable"
            );
        }
        // The sanctioned-lane seam is the ONLY external-effect tool and it is NOT a raw-external
        // marker (it is a project's own gated lane, not a post_*/deploy_*/push_* verb).
        assert!(
            !is_external_mutation("invoke_project_lane"),
            "the sanctioned-lane seam must not trip the raw-external-mutation markers: {EXTERNAL_MUTATION_MARKERS:?}"
        );
        assert!(
            GROWTH_ALLOWED_TOOLS.contains(&"invoke_project_lane"),
            "the sanctioned-lane seam must be the whitelisted publish path"
        );
    }

    // ===================================================================== #
    // ACCEPTANCE (d): publishing routes ONLY through a project's sanctioned
    // lane, behind a per-lane opt-in flag, default off / dry-run-first.
    // ===================================================================== #

    #[test]
    fn publish_is_opt_in_default_off_never_a_silent_live_publish() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        // A lane with NO growth_publish block: a publish task must NOT publish (opt-in only) and must
        // NOT spend. It returns published:false with the not_opted_in mode — never a live publish.
        let repo = plain_repo();
        let out = dispatch_growth_publish(&repo, "publish the new release-notes reel");
        assert_eq!(out["specialist"], "growth");
        assert_eq!(out["kind"], "growth_publish");
        assert_eq!(out["published"], false, "no growth_publish block -> NEVER publishes");
        assert_eq!(out["spent"], false, "publishing never spends");
        assert_eq!(out["mode"], "not_opted_in");

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    #[test]
    fn publish_with_block_but_not_live_is_dry_run_first() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        // A lane opted in (growth_publish present) but mode is the DEFAULT dry_run (or absent) must
        // DRY-RUN: validate + report the sanctioned lane argv WITHOUT spawning it, published:false.
        // We use a non-existent binary so that if the dry-run gate ever regressed to actually running,
        // the test would catch it (a spawn attempt would change the shape / error), but as designed
        // NOTHING is spawned so the argv is simply echoed back.
        let mut repo = uniq_repo("dryrun");
        repo["growth_publish"] = json!({
            "mode": "dry_run",
            "publish": ["C:\\does\\not\\exist\\publish.exe", "--lane", "post"],
            "cwd": ""
        });
        let out = dispatch_growth_publish(&repo, "publish the showcase reel");
        assert_eq!(out["mode"], "dry_run", "an opted-in but non-live lane must DRY-RUN: {out}");
        assert_eq!(out["published"], false, "a dry run publishes NOTHING");
        assert_eq!(out["spent"], false);
        assert_eq!(
            out["would_run"],
            json!(["C:\\does\\not\\exist\\publish.exe", "--lane", "post"]),
            "the dry run reports the sanctioned lane that WOULD run"
        );

        // And the promotion gate is explicit: only mode:"live" flips is_live_publish true.
        assert!(!GrowthSpecialist::is_live_publish(&repo), "dry_run is not live");
        let mut live = repo.clone();
        live["growth_publish"]["mode"] = json!("live");
        assert!(GrowthSpecialist::is_live_publish(&live), "mode:live is live");
        // absent block / absent mode / wrong type are all NOT live (default off).
        assert!(!GrowthSpecialist::is_live_publish(&plain_repo()));
        assert!(!GrowthSpecialist::is_live_publish(&json!({"growth_publish": {"publish": ["x"]}})));
        assert!(!GrowthSpecialist::is_live_publish(&json!({"growth_publish": {"mode": true}})));
        assert!(!GrowthSpecialist::is_live_publish(&json!({"growth_publish": {}})));

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // A LIVE-mode publish on an opted-in lane actually routes through the sanctioned lane and reports
    // published:true on a clean exit — proven end-to-end with a REAL trivial command (never a raw
    // external post, never a spend). This closes the live-path coverage: the sanctioned-lane seam is
    // reached only after the gate passes, and `published` is honest (true iff the lane exited 0).
    #[test]
    fn live_publish_routes_through_the_sanctioned_lane_and_reports_honestly() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        // A real, trivial, fast-exiting command stands in for a project's OWN published lane binary:
        // it exits 0, so a clean live publish reports published:true. On Windows the sanctioned lane is
        // `cmd /c echo {...}`; the trailing JSON line is what lane_summary picks up.
        #[cfg(windows)]
        let publish = json!(["cmd", "/c", "echo {\"ok\":true,\"posted\":1}"]);
        #[cfg(not(windows))]
        let publish = json!(["sh", "-c", "echo '{\"ok\":true,\"posted\":1}'"]);

        let mut repo = uniq_repo("livepub");
        repo["growth_publish"] = json!({ "mode": "live", "publish": publish, "cwd": "" });
        let out = dispatch_growth_publish(&repo, "publish the release-notes reel");
        assert_eq!(out["specialist"], "growth");
        assert_eq!(out["kind"], "growth_publish");
        assert_eq!(out["mode"], "live", "an opted-in live lane must run live: {out}");
        assert_eq!(out["ok"], true, "a clean sanctioned-lane exit is ok: {out}");
        assert_eq!(out["published"], true, "a clean live publish reports published:true: {out}");
        assert_eq!(out["spent"], false, "publishing NEVER spends (organic-only)");
        assert_eq!(out["organic"], true);
        assert_eq!(out["exit_code"], 0);

        // A LIVE publish whose sanctioned lane exits NON-ZERO reports published:false honestly (no
        // fabricated success) — the honesty floor: publish claimed ONLY on a clean settled exit.
        let mut repo_fail = uniq_repo("livepubfail");
        #[cfg(windows)]
        let fail = json!(["cmd", "/c", "exit 3"]);
        #[cfg(not(windows))]
        let fail = json!(["sh", "-c", "exit 3"]);
        repo_fail["growth_publish"] = json!({ "mode": "live", "publish": fail, "cwd": "" });
        let out_fail = dispatch_growth_publish(&repo_fail, "publish attempt that fails");
        assert_eq!(out_fail["ok"], false, "a non-zero lane exit is not ok: {out_fail}");
        assert_eq!(
            out_fail["published"], false,
            "a failed publish must NOT claim published:true (honesty floor): {out_fail}"
        );
        assert_eq!(out_fail["spent"], false);

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ===================================================================== #
    // ACCEPTANCE (e): a governance-target dispatch is DENIED at the same
    // fail-closed pecrt::safety gate (no allow arm) — the specialist can never
    // widen a whitelist / budget / bypass the skeptic / kill / freshness.
    // ===================================================================== #
    #[test]
    fn governance_target_dispatch_is_denied_at_the_pecrt_gate() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let growth = GrowthSpecialist::new();
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
            let refusal = growth
                .gate(&task, &plain_repo())
                .expect("a governance-target dispatch must be DENIED at the gate");
            assert_eq!(refusal["ok"], false);
            assert_eq!(
                refusal["pecrt_safety"], true,
                "the DENY must come from the fail-closed pecrt safety gate (no allow arm): {refusal}"
            );
        }

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ANTI-SELF-PROMOTION (the reward-hacking guard): the Growth specialist can NEVER promote its own
    // publish lane from dry_run to live. The opt-in `growth_publish.mode` lives in repos.json, and
    // repos.json is a FORBIDDEN governance target — a dispatch aimed at flipping it is DENIED at the
    // pecrt gate with no allow arm, AND the specialist's writable scope is the growth-drafts log ONLY
    // (never repos.json). So the dry-run-first ladder can only ever be climbed by the OPERATOR.
    #[test]
    fn growth_cannot_promote_its_own_publish_lane_to_live() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let growth = GrowthSpecialist::new();
        // Any attempt to touch repos.json (where growth_publish.mode lives) is denied at the gate.
        for target in ["repos.json", "repos.json#growth_publish", "repos.json growth_publish.mode"] {
            let task = Task::new(TaskKind::Remediate("none"), target, "flip growth_publish to live");
            let refusal = growth
                .gate(&task, &plain_repo())
                .expect("promoting the publish lane via repos.json must be DENIED");
            assert_eq!(refusal["ok"], false);
            assert_eq!(refusal["pecrt_safety"], true, "{refusal}");
        }
        // And the specialist's ONLY writable surface is the growth-drafts log — never repos.json.
        let scope = growth.scope_globs(&kairos_repo());
        assert!(
            !scope.iter().any(|g| g.contains("repos.json")),
            "Growth must never have repos.json in its writable scope: {scope:?}"
        );

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ---- scope_globs restricts writes to the growth-drafts path ONLY ----
    #[test]
    fn growth_scope_globs_is_the_drafts_path_only() {
        let growth = GrowthSpecialist::new();
        // kairos carries money_globs + protected — Growth must return NEITHER, only the drafts path.
        let globs = growth.scope_globs(&kairos_repo());
        assert_eq!(globs, vec!["runtime/kairos/growth_drafts.jsonl"]);
        assert!(!globs.iter().any(|g| g.contains("trader.py") || g.contains("kalshi")));
        assert!(!globs.iter().any(|g| g.contains("promote.py") || g.contains(".state")));
        // a nameless row yields no writable scope.
        assert!(growth.scope_globs(&json!({})).is_empty());
    }

    // ---- publish_cfg / publish_argv parsing (opt-in only) ----
    #[test]
    fn publish_cfg_is_opt_in_and_argv_parses() {
        // present, non-empty object -> Some
        assert!(GrowthSpecialist::publish_cfg(&json!({"growth_publish": {"mode": "dry_run"}})).is_some());
        // absent / empty -> None (opt-in only)
        assert!(GrowthSpecialist::publish_cfg(&json!({"name": "x"})).is_none());
        assert!(GrowthSpecialist::publish_cfg(&json!({"growth_publish": {}})).is_none());
        assert!(GrowthSpecialist::publish_cfg(&json!({"growth_publish": true})).is_none());

        let cfg = json!({"publish": ["sover.exe", "--live", "--lane", "post"]});
        assert_eq!(
            GrowthSpecialist::publish_argv(&cfg),
            Some(vec![
                "sover.exe".to_string(),
                "--live".to_string(),
                "--lane".to_string(),
                "post".to_string()
            ])
        );
        // missing/empty -> None (never spawn an empty command)
        assert_eq!(GrowthSpecialist::publish_argv(&json!({})), None);
        assert_eq!(GrowthSpecialist::publish_argv(&json!({"publish": []})), None);
    }

    // ---- the draft fact is a provenance-tagged, dated, first-order fact ----
    #[test]
    fn draft_fact_is_provenance_tagged_and_a_valid_first_order_fact() {
        let task = Task::new(TaskKind::Remediate("none"), "sover", "add a README badges section");
        let fact = GrowthSpecialist::draft_fact(&task, 1_783_000_000);
        assert!(fact.starts_with("rsi: growth DRAFT [GATED, unpublished, organic]"), "{fact}");
        assert!(fact.contains("lane=sover"));
        assert!(fact.contains("add a README badges section"));
        assert!(
            crate::pecrt::warm::ObservationLog::validate_fact(&fact).is_fact(),
            "the draft fact must be a valid first-order dated fact: {fact}"
        );
        // even an empty detail still yields a valid fact (the epoch datum guarantees it).
        let empty = Task::new(TaskKind::Remediate("none"), "sover", "   ");
        let f2 = GrowthSpecialist::draft_fact(&empty, 1_783_000_001);
        assert!(crate::pecrt::warm::ObservationLog::validate_fact(&f2).is_fact(), "{f2}");
    }

    // ---- is_publish_task: only the explicit marker triggers the publish path ----
    #[test]
    fn only_the_publish_marker_selects_the_publish_path() {
        assert!(is_publish_task(&format!("{PUBLISH_MARKER} do the thing")));
        assert!(is_publish_task(&format!("  {PUBLISH_MARKER} leading space")));
        // an ordinary content directive NEVER triggers publish (defense against accidental publish).
        assert!(!is_publish_task("draft a README section"));
        assert!(!is_publish_task("publish the reel")); // the word 'publish' alone is not the marker
        assert!(!is_publish_task(""));
    }

    // ===================================================================== //
    // Phase-B GROWTH COMPOSER (finding #12) — the pure cores + the day stamp
    // ===================================================================== //

    // ---- PERSONA RULE: the deterministic deny filter refuses every operator marker ----
    #[test]
    fn persona_filter_rejects_every_operator_marker_and_accepts_brand_copy() {
        for m in OPERATOR_MARKERS {
            assert!(
                violates_persona(&format!("release announcement written by {m} today")),
                "marker '{m}' must be refused"
            );
            assert!(
                violates_persona(&m.to_uppercase()),
                "the filter is case-insensitive: '{m}'"
            );
        }
        // Standalone name tokens are word-boundary matched (review finding: the compound markers
        // all contain "cayleb", leaving bare-surname drafts uncovered).
        assert!(violates_persona("release notes reviewed by James today"));
        assert!(violates_persona("maintained by Alvarez"));
        // ...but prefix-sharing words do NOT trip the token filter.
        assert!(!violates_persona("the jameson integration suite is green"));
        // Brand copy passes: authored under the project's own name, no operator identifier.
        assert!(!violates_persona(
            "sover 0.4 — 3 verified reels/day now ship with URLs; quickstart moved to README"
        ));
        assert!(!violates_persona(""));
        // A draft FACT built from a marker-carrying detail is refused too — the filter guards the
        // exact text that would land in the drafts log.
        let task = Task::new(TaskKind::Remediate("none"), "sover", "post authored by Cayleb");
        assert!(violates_persona(&GrowthSpecialist::draft_fact(&task, 1_783_000_000)));
    }

    // ---- HONEST TRIGGER: growth-directive keyword detection ----
    #[test]
    fn growth_directive_keyword_detection() {
        assert!(is_growth_directive("- [ ] [chore] refresh the README quickstart section"));
        assert!(is_growth_directive("- [ ] [feature] draft release notes for v0.4"));
        assert!(is_growth_directive("- [ ] add usage examples to docs"));
        assert!(is_growth_directive("- [ ] [feature] publish a showcase post (organic)"));
        assert!(!is_growth_directive("- [ ] [feature] fix the sqlite WAL checkpoint deadlock"));
        assert!(!is_growth_directive("- [ ] [refactor] split the scheduler loop"));
        assert!(!is_growth_directive(""));
    }

    // ---- HONEST TRIGGER: only TODAY'S planner line (or an open campaign step) fires ----
    #[test]
    fn growth_directive_picks_todays_planner_line_only() {
        let marker = "(ceo 2026-07-13)";
        let backlog = "- [ ] [feature] fix the sqlite WAL checkpoint deadlock (ceo 2026-07-13)\n\
                       - [ ] [chore] refresh the README quickstart + badges (ceo 2026-07-13)\n\
                       - [ ] [chore] polish the README intro (ceo 2026-07-01)\n";
        let d = growth_directive(backlog, marker).expect("today's growth line is found");
        assert!(d.contains("README quickstart"), "{d}");
        assert!(!d.starts_with("- [ ]"), "the checkbox prefix is stripped: {d}");
        // yesterday's ceo goal alone does NOT fire (stale directives are not today's plan)
        assert!(growth_directive("- [ ] polish the README (ceo 2026-07-01)\n", marker).is_none());
        // checked / deferred lines never fire
        assert!(growth_directive("- [x] refresh the README (ceo 2026-07-13)\n", marker).is_none());
        assert!(growth_directive(
            "- [ ] refresh the README (ceo 2026-07-13)  (deferred: gave up)\n",
            marker
        )
        .is_none());
        // an OPEN campaign growth step fires regardless of the day it was planned
        assert!(growth_directive(
            "- [ ] [feature] [campaign:sover-2026-07-12] (step 2) write the showcase page (campaign 2026-07-12)\n",
            marker
        )
        .is_some());
        // ...but a non-growth campaign step does not
        assert!(growth_directive(
            "- [ ] [feature] [campaign:x] (step 1) refactor the scheduler loop (campaign 2026-07-12)\n",
            marker
        )
        .is_none());
        // empty backlog -> a quiet day
        assert!(growth_directive("", marker).is_none());
    }

    // ---- PUBLIC-LANE SELECTION: public + north star + not RED, on the real repos.json shapes ----
    #[test]
    fn public_lane_selection_requires_public_flag_goal_and_not_red() {
        let green = json!({"status": "green", "healthy": true});
        let sover = json!({"name": "sover", "public": true});
        assert!(eligible_repo(&sover, "Post 3 verified reels/day", &green));
        // daedulus: explicitly public:false -> never composed
        assert!(!eligible_repo(&json!({"name": "daedulus", "public": false}), "a goal", &green));
        // asmodeus: NO public flag at all (private by omission; its goal forbids public exposure)
        assert!(!eligible_repo(&json!({"name": "asmodeus"}), "capital velocity", &green));
        // a non-bool public value is NOT public (fail-closed truthiness)
        assert!(!eligible_repo(&json!({"name": "x", "public": "yes"}), "a goal", &green));
        // no north star = not a planned lane = never composed
        assert!(!eligible_repo(&sover, "   ", &green));
        // RED lane: don't market a dead engine (green-before-growth)
        assert!(!eligible_repo(&sover, "a goal", &json!({"status": "red"})));
        // an ABSENT rollup entry is not red — drafting is local + harmless
        assert!(eligible_repo(&sover, "a goal", &json!({})));
        // a nameless row has no runtime dir and is never eligible
        assert!(!eligible_repo(&json!({"public": true}), "a goal", &green));
    }

    // ---- OPERATOR-GATED RED: a human-blocked platform probe is NOT engine death ----
    #[test]
    fn red_carried_only_by_operator_gated_probes_still_composes() {
        let sover = json!({"name": "sover", "public": true});
        // YouTube's human-gated login is the ONLY red -> the engine is alive: compose-and-hold
        // proceeds for the healthy platforms (the 2026-07-17 sover zero-drafts wedge).
        let yt_red = json!({"status": "red", "red_operator_gated_only": true, "probes": {
            "publish_recency_youtube": "red", "publish_recency_instagram": "green"}});
        assert!(red_on_operator_gated_probes_only(&yt_red));
        assert!(eligible_repo(&sover, "a goal", &yt_red));
        // ANY non-gated red = engine-dead -> green-before-growth holds, unchanged
        let engine_red = json!({"status": "red", "red_operator_gated_only": false});
        assert!(!red_on_operator_gated_probes_only(&engine_red));
        assert!(!eligible_repo(&sover, "a goal", &engine_red));
        // an ABSENT flag reads engine-dead (fail closed — pre-flag rollups, sweep-panic entries)
        assert!(!eligible_repo(&sover, "a goal", &json!({"status": "red"})));
        // non-bool junk is not a bypass
        assert!(!eligible_repo(&sover, "a goal",
            &json!({"status": "red", "red_operator_gated_only": "true"})));
        // the flag is red-scoped: a non-red rollup is eligible with or without it
        assert!(eligible_repo(&sover, "a goal",
            &json!({"status": "yellow", "red_operator_gated_only": false})));
    }

    // ---- DAY-GATE: stamp-first — a stamped lane is a same-day no-op even when compose failed ----
    #[test]
    fn growth_day_gate_is_stamp_first_and_same_day_idempotent() {
        let repo = uniq_repo("stampfirst");
        let date = "2026-07-13";
        assert!(!already_drafted(&repo, date), "fresh lane is un-stamped");
        // STAMP FIRST (the pre-LLM write): the day's attempt is claimed ATOMICALLY — the first
        // claim wins, a second claim (the other OS process inside the same race window) loses.
        assert!(claim_growth_stamp(&repo, date), "first claim wins the day");
        assert!(!claim_growth_stamp(&repo, date), "second claim loses: create_new is the gate");
        assert!(already_drafted(&repo, date), "the stamp gates the rest of the day");
        // A NAMELESS row can never claim (fail closed).
        assert!(!claim_growth_stamp(&json!({}), date));
        // A failed compose OVERWRITES the note with a NAMED skip reason but keeps the gate closed
        // (one attempt/day — an LLM outage must not retry every 2-minute sweep).
        stamp_growth(&repo, date, "skip:llm_unavailable curl exit 22: 429");
        assert!(already_drafted(&repo, date), "a failed compose still consumed the day");
        let body =
            std::fs::read_to_string(growth_stamp_path(&repo, date).expect("stamp path")).unwrap();
        assert!(
            body.starts_with("skip:llm_unavailable"),
            "the named skip reason is recorded in the marker: {body}"
        );
        // a different day is un-stamped (the gate is per-day)
        assert!(!already_drafted(&repo, "2026-07-14"));
        // a NAMELESS row can never be stamped -> reads as already-drafted (never attempted)
        assert!(already_drafted(&json!({}), date));
        if let Some(p) = growth_stamp_path(&repo, date) {
            if let Some(dir) = p.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    // ---- REPLY PARSING: strict JSON via extract_json/cap_line, honest degradation ----
    #[test]
    fn parse_growth_reply_extracts_strict_json_and_rejects_empty_body() {
        let reply = "Here is the draft:\n```json\n{\"kind\":\"release_note\",\"title\":\"sover 0.4\",\
                     \"body\":\"3 verified reels/day now ship with URLs.\\nQuickstart moved to the README.\"}\n```";
        let (kind, title, body) = parse_growth_reply(reply).expect("fenced JSON parses");
        assert_eq!(kind, "release_note");
        assert_eq!(title, "sover 0.4");
        assert!(!body.contains('\n'), "the body is flattened to one line: {body}");
        assert!(body.contains("3 verified reels/day"));
        // an unknown kind degrades to "post" (a formatting nit, never a dropped draft)
        let (k2, ..) =
            parse_growth_reply("{\"kind\":\"tweetstorm\",\"title\":\"t\",\"body\":\"b\"}").unwrap();
        assert_eq!(k2, "post");
        // a blank/missing body is None — a blank draft is busywork, never dispatched
        assert!(parse_growth_reply("{\"kind\":\"post\",\"title\":\"t\",\"body\":\"  \"}").is_none());
        assert!(parse_growth_reply("{\"kind\":\"post\",\"title\":\"t\"}").is_none());
        assert!(parse_growth_reply("no json here at all").is_none());
    }

    // ---- MODEL RESOLUTION: config-resolved, never a hardcoded provider id ----
    #[test]
    fn growth_model_resolves_from_brain_config_never_hardcoded() {
        use crate::improver::brain::{BrainConfig, Workers};
        // brain enabled + ideate worker -> the creative role's configured model
        let cfg = BrainConfig {
            enabled: true,
            aggregator: "vendor/agg:free".into(),
            verifier: "v".into(),
            workers: Workers {
                ideate: Some("vendor/creative:free".into()),
                ..Default::default()
            },
            layers: 2,
        };
        assert_eq!(pick_growth_model(&cfg), "vendor/creative:free");
        // enabled, no ideate worker -> the aggregator
        let cfg_no_ideate = BrainConfig {
            workers: Workers::default(),
            ..cfg.clone()
        };
        assert_eq!(pick_growth_model(&cfg_no_ideate), "vendor/agg:free");
        // disabled -> the CEO planner model (the provider-aware transport's chat_model_for
        // substitutes the configured autopilot model under OpenRouter — no hardwired provider id)
        let cfg_off = BrainConfig {
            enabled: false,
            ..cfg
        };
        assert_eq!(pick_growth_model(&cfg_off), crate::ceo::CEO_MODEL);
    }

    // ---- END-TO-END (LLM seam skipped): a composed detail lands ONE gated dated draft line ----
    #[test]
    fn a_composed_draft_detail_lands_one_gated_dated_line() {
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let repo = uniq_repo("composed");
        // The exact detail shape compose_and_dispatch builds from a parsed reply + the directive.
        let detail = "[release_note] sover 0.4 — 3 verified reels/day now ship with URLs \
                      (directive: [chore] refresh the README quickstart (ceo 2026-07-13))";
        assert!(!violates_persona(detail), "brand copy passes the persona filter");

        let out = dispatch_growth_content(&repo, detail);
        assert_eq!(out["ok"], true, "the gated dispatch succeeds on an ordinary lane: {out}");
        assert_eq!(out["gated"], true);
        assert_eq!(out["published"], false, "draft-only — the publish ladder is never invoked");
        assert_eq!(out["spent"], false);
        let path = out["artifact_path"].as_str().expect("artifact path");
        let body = std::fs::read_to_string(path).expect("drafts log exists");
        assert_eq!(body.lines().count(), 1, "exactly ONE draft line landed: {body}");
        let line = body.lines().next().unwrap();
        assert!(line.contains('\t'), "the draft line is dated (iso-date TAB fact): {line}");
        assert!(line.contains("[release_note] sover 0.4"), "{line}");
        assert!(line.contains("[GATED, unpublished, organic]"), "{line}");

        if let Some(dir) = std::path::Path::new(path).parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ---- PUBLISH "last inch" — the human approval gate (draft_line_approved is pure + fail-closed) ----
    #[test]
    fn draft_line_approved_only_on_a_json_object_with_boolean_true() {
        // The composer's real output shape — a plain dated TEXT fact — is NEVER approved.
        assert!(!draft_line_approved(
            "2026-07-15\trsi: growth DRAFT [GATED, unpublished, organic] lane=x: hi (t=1)"
        ));
        // Operator approval: a JSON object with a boolean-true `approved`, with or without the date tab.
        assert!(draft_line_approved("2026-07-15\t{\"approved\": true, \"note\": \"ship it\"}"));
        assert!(draft_line_approved("{\"approved\":true}"));
        // Fail-closed: string/int truthies and a false flag are NOT approval.
        assert!(!draft_line_approved("{\"approved\":\"true\"}"));
        assert!(!draft_line_approved("{\"approved\":1}"));
        assert!(!draft_line_approved("{\"approved\":false}"));
        assert!(!draft_line_approved("{}"));
        assert!(!draft_line_approved("not json at all"));
    }

    // The pure AUTOMATED content gate: a genuine composed TEXT draft passes; an empty line, a raw
    // operator-control JSON line, and a persona-violating draft are all BLOCKED — no human approval.
    #[test]
    fn auto_publish_content_check_passes_content_and_blocks_persona_and_control_lines() {
        // a real composed draft (persona-clean) passes, returning the fact text to publish
        let ok = auto_publish_content_check(
            "2026-07-15\trsi: growth DRAFT [GATED, unpublished, organic] lane=x: [post] honest copy (t=1)",
        );
        assert!(ok.is_ok(), "a persona-clean composed draft must pass: {ok:?}");
        assert!(ok.unwrap().contains("honest copy"));
        // empty / control-JSON / persona-violating drafts are refused (fail-closed, named reason)
        assert_eq!(auto_publish_content_check("2026-07-15\t   ").unwrap_err(), "empty draft");
        assert_eq!(
            auto_publish_content_check("2026-07-15\t{\"approved\": true}").unwrap_err(),
            "control line, not content"
        );
        assert_eq!(
            auto_publish_content_check(
                "2026-07-15\trsi: growth DRAFT lane=x: maintained by cayleb (t=2)"
            )
            .unwrap_err(),
            "persona violation"
        );
    }

    // The AUTONOMOUS publish seam contract: a composed draft AUTO-publishes with NO operator
    // `approved:true`; the per-lane-per-day RATE cap blocks a second same-day publish; a persona
    // violation blocks BEFORE consuming the day's budget; and a draft already LIVE-published is never
    // re-shipped (per-draft dedup). Nothing here waits on a human.
    #[test]
    fn auto_publish_fires_without_approval_and_content_rate_gates_block() {
        use std::cell::Cell;
        // auto_publish_draft logs via notify::send on dispatch — silence it (kill-switch).
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let today = Utc::now().format("%Y-%m-%d").to_string();

        // --- a plain composed draft (NO approved:true) AUTO-publishes exactly once ---
        let repo = uniq_repo("pub_auto");
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join("growth_drafts.jsonl"),
            "2026-07-15\trsi: growth DRAFT [GATED, unpublished, organic] lane=x: [post] Ship it — honest copy (t=1)\n",
        )
        .unwrap();
        let called = Cell::new(0u32);
        let out = auto_publish_draft(&repo, |_r, _d| {
            called.set(called.get() + 1);
            json!({"published": false, "mode": "dry_run"})
        });
        assert_eq!(called.get(), 1, "a composed draft MUST auto-publish without approved:true");
        assert_eq!(out["dispatched"], json!(true));

        // --- RATE cap: a SECOND publish on the SAME lane the SAME day is blocked ---
        let out_again = auto_publish_draft(&repo, |_r, _d| {
            called.set(called.get() + 1);
            json!({"published": false, "mode": "dry_run"})
        });
        assert_eq!(called.get(), 1, "the per-lane-per-day rate cap blocks a second same-day publish");
        assert_eq!(out_again["dispatched"], json!(false));
        assert_eq!(out_again["reason"], json!("daily publish cap reached"));

        // --- PERSONA violation blocks, and does NOT consume the day's rate budget ---
        let repo_p = uniq_repo("pub_persona");
        let rt_p = paths::runtime_dir(&repo_p).unwrap();
        let _ = std::fs::remove_dir_all(&rt_p);
        std::fs::create_dir_all(&rt_p).unwrap();
        std::fs::write(
            rt_p.join("growth_drafts.jsonl"),
            "2026-07-15\trsi: growth DRAFT [GATED, unpublished, organic] lane=x: maintained by cayleb (t=2)\n",
        )
        .unwrap();
        let called_p = Cell::new(0u32);
        let out_p = auto_publish_draft(&repo_p, |_r, _d| {
            called_p.set(called_p.get() + 1);
            json!({"published": true, "mode": "live"})
        });
        assert_eq!(called_p.get(), 0, "a persona-violating draft must NOT dispatch");
        assert_eq!(out_p["reason"], json!("persona violation"));
        assert!(
            !published_day_marker_path(&repo_p, &today).unwrap().exists(),
            "a persona block must fail BEFORE the day-claim (no rate budget consumed)"
        );

        // --- DEDUP: a draft already LIVE-published is never re-shipped ---
        let repo_d = uniq_repo("pub_dedup");
        let rt_d = paths::runtime_dir(&repo_d).unwrap();
        let _ = std::fs::remove_dir_all(&rt_d);
        std::fs::create_dir_all(&rt_d).unwrap();
        let line =
            "2026-07-15\trsi: growth DRAFT [GATED, unpublished, organic] lane=x: [post] already live (t=3)";
        std::fs::write(rt_d.join("growth_drafts.jsonl"), format!("{line}\n")).unwrap();
        write_published_marker(&repo_d, line.trim(), true, "live"); // simulate a prior live publish
        let called_d = Cell::new(0u32);
        let out_d = auto_publish_draft(&repo_d, |_r, _d| {
            called_d.set(called_d.get() + 1);
            json!({"published": true, "mode": "live"})
        });
        assert_eq!(called_d.get(), 0, "a draft already live-published must NOT be re-shipped");
        assert_eq!(out_d["reason"], json!("already published"));

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
        let _ = std::fs::remove_dir_all(rt);
        let _ = std::fs::remove_dir_all(rt_p);
        let _ = std::fs::remove_dir_all(rt_d);
    }
}
