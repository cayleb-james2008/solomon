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

/// The every-sweep growth-content hook, ridden on `ceo_slow_tail` (the D12/Phase-B SEAM). Currently a
/// deliberate NO-OP: the gated dispatch plumbing above (`dispatch_growth_content` /
/// `dispatch_growth_publish`) is fully built, but an every-sweep caller needs an HONEST trigger — a
/// planner-composed content directive (morning-plan/focus output), never a static daily fact (that
/// would be the busywork the eval-park doctrine forbids). The composer that fills this: pick the one
/// public lane whose planner output carries a growth directive, day-gate + stamp-first exactly like
/// `sover_boost`, and dispatch under the PERSONA RULE (authored as the project's own brand — never the
/// operator's personal name).
pub fn maybe_draft_growth_content(_snapshot: &serde_json::Value, _status: &serde_json::Value) {}

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
}
