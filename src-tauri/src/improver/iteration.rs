//! Native Rust port of run_improver.py's `one_iteration()` — the ~558-line per-iteration
//! orchestrator (source lines ~2154-2712), the core RSI state machine.
//!
//! Bug-for-bug with improver/run_improver.py. Every status / phase / reason string, every log line,
//! and every branch-name template is reproduced VERBATIM (em-dashes, arrows, and unicode included) —
//! the dashboard and the agent's corrective prompts both key on these exact phrases.
//!
//! The Python module-level GLOBALS are fields on the one [`Ctx`] threaded through the loop; every
//! helper one_iteration calls is a function on a leaf module under `crate::improver::*`
//! (`pi`/`gitops`/`gates`/`escalation`/`backlog`/`ship`/`phases`/`visual`). The two phases that live
//! ONLY inside one_iteration's call graph and are not provided by any leaf module — `ideate_phase`
//! (+ its `ideate`/`ideate_task`/`parse_ideas`/`recent_history_summaries` support) and
//! `solomon_task` — are ported here as private functions (with their own #[cfg(test)] coverage for
//! the pure parsers). Their lesson-dedup helpers (`tokenize`/`is_novel`/`read_lessons`) are private
//! copies, identical to the ones phases.rs keeps private, so this module is self-contained.
//!
//! ORDER OF THE REVERT GATES (each `drop_branch` with its EXACT reason on failure), reproduced from
//! the source: gate (test) -> anti-gaming -> cross-repo -> eval -> leak -> review -> visual, then
//! ship on success, then the escalation ladder on failure.
//!
//! `register_failure`'s `limit` is the Python default (3) for `_note_noop`/`_note_deviation`/
//! `_note_revert` (the Rust signatures take it explicitly; the source passes none == 3).

use serde_json::{json, Value};

use crate::improver::ctx::{self, Ctx};
use crate::improver::{backlog, escalation, gates, gitops, phases, pi, ship, visual};

use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// The `limit` argument the source's `_note_noop`/`_note_deviation`/`_note_revert` default to (3).
const NOTE_LIMIT: i64 = 3;

// --------------------------------------------------------------------------- #
// one_iteration — the per-iteration state machine (run_improver ~2154-2709)
// --------------------------------------------------------------------------- #

/// run_improver.one_iteration (~2154-2709): one full RSI iteration — preflight (clean/sync the base,
/// branch, base-gate), implement (build_task + run_pi, with the ideate/plan phases when enabled), the
/// ordered revert gates, and ship-or-escalate. Writes heartbeat + history at each phase with the
/// EXACT status/phase/reason vocabulary. BEAUTIFY (docs-only) and SOLOMON (supervisor fix-session)
/// behave differently (different branch prefix, goal, system prompt, and gate-skips), as in the
/// source. The iteration counter is incremented ONLY once the implement phase is actually reached.
pub fn one_iteration(ctx: &mut Ctx) {
    // branch = "rsi/beautify-<stamp>" / "rsi/solomon-<stamp>" / "rsi/iter-<stamp>"
    let stamp = ctx::stamp();
    let branch = if ctx.beautify {
        format!("rsi/beautify-{stamp}")
    } else if ctx.solomon {
        format!("rsi/solomon-{stamp}")
    } else {
        format!("rsi/iter-{stamp}")
    };

    let cur_branch = ctx
        .git(&["rev-parse", "--abbrev-ref", "HEAD"], 120)
        .stdout
        .trim()
        .to_string();
    let base_branch = ctx.base_branch.clone();

    // AUTO-RECOVER: a dirty BASE tree, or operator-looking untracked files on the base — stash them
    // (recoverable) instead of wedging the loop, but ONLY on the base branch.
    if cur_branch == base_branch {
        let untracked = gitops::untracked_non_ignored_files(ctx);
        let artifact_pats = gitops::repo_artifact_patterns(ctx, &ctx.name.clone());
        let refuse_untracked = !untracked.is_empty()
            && gitops::untracked_recovery_action(ctx, &untracked, Some(&artifact_pats)) == "refuse";
        if (gitops::tree_dirty(ctx) || refuse_untracked) && gitops::auto_stash_base(ctx, &branch) {
            ctx.log(
                "auto-recover: base tree was dirty/untracked — stashed (recoverable) and resuming",
            );
        }
    }

    if gitops::dirty_blocks_iteration(ctx, gitops::tree_dirty(ctx), &cur_branch, &base_branch) {
        // After N consecutive dirty-base bails, self-stop (write STOP + an error heartbeat).
        if escalation::note_dirty_base_bail(ctx, gitops::tree_dirty(ctx), &cur_branch, &base_branch)
        {
            ctx.log(&format!(
                "SELF-STOP: dirty base '{base_branch}' persisted for {} \
consecutive preflight bails — writing STOP + error heartbeat (operator action required)",
                3 // _DIRTY_BASE_PERSISTENT_LIMIT
            ));
            return;
        }
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "preflight",
            "last_summary": format!(
                "Working tree is dirty on the base branch '{base_branch}' — commit or stash your \
        changes; the loop won't clobber base-branch work."
            ),
        }));
        ctx.log(&format!(
            "SKIP iteration: working tree dirty on base branch '{base_branch}'"
        ));
        return;
    }
    // a clean iteration resets the persistent-dirty-base counter
    escalation::note_dirty_base_bail(ctx, gitops::tree_dirty(ctx), &cur_branch, &base_branch);

    // Robust preflight: FORCE back to the base branch.
    let co = ctx.git(&["checkout", "--force", &base_branch], 120);
    if co.code != 0 {
        let detail: String = co.stderr.trim().chars().take(200).collect();
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "preflight",
            "last_summary": format!("Could not checkout {base_branch}: {detail}"),
        }));
        ctx.log(&format!(
            "checkout {base_branch} failed — skipping iteration"
        ));
        return;
    }
    ctx.git(&["reset", "--hard"], 120); // drop tracked changes from a dead run

    // GUARD the `git clean -fd` — never silently delete operator/untracked work.
    let untracked = gitops::untracked_non_ignored_files(ctx);
    if !untracked.is_empty() {
        let artifact_pats = gitops::repo_artifact_patterns(ctx, &ctx.name.clone());
        let action = gitops::untracked_recovery_action(ctx, &untracked, Some(&artifact_pats));
        if action == "refuse" {
            let head: Vec<String> = untracked.iter().take(8).cloned().collect();
            let more = if untracked.len() > 8 {
                format!(" (+{} more)", untracked.len() - 8)
            } else {
                String::new()
            };
            ctx.heartbeat(json!({
                "status": "error",
                "phase": "preflight",
                "last_summary": format!(
                    "Untracked non-ignored files on '{base_branch}' would be deleted by the preflight \
clean — the loop won't destroy possible operator work. Commit, stash, or remove them: {}{}",
                    head.join(", "),
                    more
                ),
            }));
            ctx.log(&format!(
                "REFUSE preflight clean: {} untracked non-ignored file(s) on {base_branch} \
— skip+escalate (won't destroy operator work)",
                untracked.len()
            ));
            return;
        }
        if action == "recover" {
            ctx.log(&format!(
                "AGENT-ARTIFACT RECOVERY: {} untracked file(s) on {base_branch} all match \
agent-artifact heuristics — staging on the rsi branch (not the base), then gate+ship/revert",
                untracked.len()
            ));
            // do NOT clean here — the files are staged AFTER the branch is cut (git add -A picks them up).
        } else {
            // action == 'none' shouldn't happen here, but be safe: clean.
            ctx.git(&["clean", "-fd"], 120);
        }
    } else {
        ctx.git(&["clean", "-fd"], 120); // safe now: no untracked non-ignored files to destroy
    }

    if ctx.has_remote() {
        let fetched = ctx.git(&["fetch", "origin", "--quiet"], 120);
        if fetched.code != 0 {
            let detail: String = fetched.stderr.trim().chars().take(160).collect();
            ctx.log(&format!(
                "preflight fetch failed (offline?): {detail} — skipping origin sync, \
iterating on local {base_branch}"
            ));
        } else {
            // REFUSE to adopt a base that moved without a gated iteration (un-pushed base commits).
            let ahead = ctx.git(
                &[
                    "rev-list",
                    "--count",
                    &format!("origin/{base_branch}..{base_branch}"),
                ],
                120,
            );
            let n_ahead: i64 = if ahead.code == 0 {
                let s = ahead.stdout.trim();
                let s = if s.is_empty() { "0" } else { s };
                s.parse::<i64>().unwrap_or(0)
            } else {
                0
            };
            let mut skip_origin_reset = false;
            if n_ahead > 0 {
                let shas = ctx
                    .git(
                        &[
                            "log",
                            &format!("origin/{base_branch}..{base_branch}"),
                            "--oneline",
                        ],
                        120,
                    )
                    .stdout
                    .trim()
                    .to_string();
                if ctx.ship == "local" {
                    ctx.log(&format!(
                        "base ahead of origin by {n_ahead}; ship=local — iterating on local {base_branch} (no reset)"
                    ));
                    escalation::note_unpushed_base_bail(ctx, false, 0, "");
                    skip_origin_reset = true;
                } else {
                    let pu = ctx.git(
                        &["push", "origin", &format!("{base_branch}:{base_branch}")],
                        120,
                    );
                    if pu.code == 0 {
                        ctx.log(&format!(
                            "auto-synced {n_ahead} un-pushed base commit(s) to origin/{base_branch} — base reconciled"
                        ));
                        escalation::note_unpushed_base_bail(ctx, false, 0, "");
                    } else if escalation::note_unpushed_base_bail(ctx, true, n_ahead, &shas) {
                        return; // self-stopped after N consecutive failed FF pushes
                    } else {
                        let pu_err: String = pu.stderr.trim().chars().take(120).collect();
                        let shas_trunc: String = shas.chars().take(240).collect();
                        ctx.heartbeat(json!({
                            "status": "error",
                            "phase": "preflight",
                            "reason": "unpushed_base",
                            "last_summary": format!(
                                "{base_branch} has {n_ahead} commit(s) not on origin and the fast-forward \
push failed ({pu_err}). Reconcile with origin; managed repos change only via gated PRs. Commits: {shas_trunc}"
                            ),
                        }));
                        ctx.log(&format!(
                            "REFUSE preflight: {n_ahead} un-pushed base commit(s) — FF push failed"
                        ));
                        return;
                    }
                }
            } else {
                escalation::note_unpushed_base_bail(ctx, false, 0, ""); // clean base: reset the bail counter
            }
            if !skip_origin_reset {
                let rs = ctx.git(&["reset", "--hard", &format!("origin/{base_branch}")], 120);
                if rs.code != 0 {
                    let detail: String = rs.stderr.trim().chars().take(160).collect();
                    ctx.log(&format!(
                        "reset to origin/{base_branch} failed: {detail} — using local {base_branch}"
                    ));
                }
            }
        }
    }

    // Branch hygiene: condense leftover rsi/* branches.
    let pruned = gitops::prune_stale_rsi_branches(ctx);
    if pruned > 0 {
        ctx.log(&format!(
            "branch hygiene: pruned {pruned} stale rsi/* branch(es) before this iteration"
        ));
    }
    if ctx.git(&["checkout", "-B", &branch], 120).code != 0 {
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "preflight",
            "last_summary": format!("Could not create branch {branch}"),
        }));
        return;
    }
    let base = gitops::head_sha(ctx);

    // Anti-gaming baseline: measure the gate on the CLEAN base before Pi touches anything.
    let mut base_tests: Value = Value::Null;
    if !ctx.beautify {
        let (bgreen, bt, _tail) = gates::run_gate(ctx);
        base_tests = bt;
        if bgreen {
            escalation::note_base_gate_red_bail(ctx, false, ""); // green base resets the persistent-red counter
        }
        if !bgreen && !ctx.solomon {
            let unrunnable = base_tests
                .get("gate_unrunnable")
                .map(value_truthy)
                .unwrap_or(false);
            let summary = if unrunnable {
                "Base gate is UNRUNNABLE (import/collection error — e.g. pytest not installed, a \
broken venv, or a wrong gate command). Fix the gate, then Start to resume."
                    .to_string()
            } else {
                format!(
                    "Base gate is RED before any change ({}). Fix the gate command or the base; \
the loop can't measure a gain from a red base.",
                    py_dict_str(&base_tests)
                )
            };
            ctx.git(&["checkout", "--force", &base_branch], 120);
            ctx.git(&["branch", "-D", &branch], 120);
            // Self-stop after N consecutive red-base bails.
            if escalation::note_base_gate_red_bail(ctx, true, &summary) {
                return;
            }
            ctx.heartbeat(json!({
                "status": "error",
                "phase": "preflight",
                "reason": if unrunnable { "gate_unrunnable" } else { "base_gate_red" },
                "last_summary": summary,
            }));
            ctx.log("base gate RED — skipping (preflight bail, not counted as an iteration)");
            return;
        }
        // A custom gate that exits 0 but prints no parseable counts: surface gate_no_counts.
        let has_any_count = is_truthy_obj(&base_tests)
            && (value_truthy(base_tests.get("passed").unwrap_or(&Value::Null))
                || value_truthy(base_tests.get("failed").unwrap_or(&Value::Null))
                || value_truthy(base_tests.get("errors").unwrap_or(&Value::Null)));
        if is_truthy_obj(&base_tests) && !ctx.gate_cmd.is_empty() && !has_any_count {
            ctx.heartbeat(json!({
                "status": "iterating",
                "phase": "preflight",
                "reason": "gate_no_counts",
                "last_summary": "custom gate emits no parseable test counts — numeric anti-gaming rails \
are INACTIVE for this repo (only returncode + skip/xfail-marker checks apply)",
            }));
            ctx.log(
                "WARNING: custom gate emitted no parseable test counts — the pass-count anti-gaming \
check is INACTIVE for this gate (only returncode + skip/xfail-marker detection apply). \
Have the gate print a pytest-style 'N passed' or unittest 'Ran N tests' summary.",
            );
        }
    }

    // Per-repo EVAL_CMD baseline.
    let mut base_eval: Option<f64> = None;
    if !ctx.beautify && !gates::eval_cmd(ctx, &ctx.name.clone()).is_empty() {
        let ev = gates::run_eval_gate(ctx, None);
        base_eval = ev.get("score").and_then(Value::as_f64);
        if let Some(score) = base_eval {
            ctx.log(&format!("eval baseline: {}", fmt_float(score)));
        }
    }

    // ---- goal / task selection ------------------------------------------- #
    let goal: String;
    let task: String;
    let system_md: Option<std::path::PathBuf>;
    let mut item_deviated = false;

    if ctx.solomon {
        goal = "supervise: diagnose and fix the persistent gate failure".to_string();
        task = solomon_task(ctx);
        system_md = Some(ctx.solomon_md.clone());
    } else if ctx.beautify {
        goal =
            "Beautify this repository (README/banner/badges/Mermaid/About, docs only)".to_string();
        task = "Beautify this repository per beautify.md — rewrite the README to modern OSS \
standards (centered banner, shields.io badges, a Mermaid architecture diagram, full sections), \
create assets/banner.svg, set the GitHub About (description + topics), and add LICENSE/CONTRIBUTING \
only if missing. Documentation and presentation only — do NOT change any source code or behavior. \
Then stop."
            .to_string();
        system_md = Some(ctx.beautify_md.clone());
    } else {
        // Clear any stale error status a PRIOR non-HALT preflight bail left in the heartbeat.
        if ctx.hb.get("status").and_then(Value::as_str) == Some("error") {
            ctx.heartbeat(json!({"status": "iterating", "phase": "preflight"}));
        }
        // IDEATE phase.
        ideate_phase(ctx);
        let (g, tier) = backlog::top_backlog_item(ctx)
            .unwrap_or_else(|| ("model-chosen improvement".to_string(), "chore".to_string()));
        // EMPTY-GOAL GUARD.
        if backlog::needs_goal_skip(ctx, &g) {
            ctx.heartbeat(json!({
                "status": "error",
                "phase": "preflight",
                "reason": "needs_goal",
                "last_summary": "This repo has no north-star GOAL set and no actionable backlog — \
            set a goal in Config so the loop has an objective (it will not fabricate work).",
            }));
            ctx.log(
                "SKIP iteration: no north-star GOAL and no actionable backlog item — needs_goal \
(set a goal in Config; the loop won't fabricate work)",
            );
            return;
        }
        escalation::apply_fallback_model(ctx, &g); // escalation rung 1
        let mut t = escalation::build_task(ctx, &g, &tier);
        // PLAN phase.
        if ctx.plan_enabled && !g.is_empty() {
            ctx.heartbeat(json!({"phase": "plan"}));
            let plan = phases::run_plan_phase(ctx, &g);
            if !plan.is_empty() {
                t.push_str(&format!(
                    "\n\n## Implementation plan (advisory — from the planning phase)\n{plan}"
                ));
            }
        }
        goal = g;
        task = t;
        system_md = None;
    }

    // REAL iteration — count it now (not at the top).
    increment_iteration(ctx);
    let n = ctx.hb.get("iteration").and_then(Value::as_i64).unwrap_or(0);
    ctx.heartbeat(json!({
        "status": "iterating",
        "phase": "implement",
        "goal": goal,
        "last_pr": Value::Null,
        "tests": Value::Null,
    }));
    ctx.log(&format!(
        "iteration {n}: branch {branch} — Pi ({}) working{}",
        ctx.pi_model,
        if ctx.beautify {
            " (beautify, docs-only)"
        } else {
            ""
        }
    ));

    let implement_timeout = if ctx.beautify {
        pi::TIMEOUT_BEAUTIFY
    } else {
        pi::TIMEOUT_IMPLEMENT
    };
    let p = pi::run_pi(ctx, &task, implement_timeout, system_md.as_deref());
    // pi::run_pi never panics: a timeout returns rc=124, a spawn failure rc<0 with empty stdout.
    // The Python TimeoutExpired -> _drop_branch(noop, "Pi session timed out.") path is reproduced
    // by detecting the rc=124 timeout marker ALONE (run_pi sets 124 only on a timeout, and pi streams
    // a partial JSONL stdout before the kill, so a real timeout almost always has non-empty stdout —
    // gating an extra `&& stdout.is_empty()` here let timed-out, half-finished work fall through to
    // the gate/ship path). Any other nonzero+empty is the generic Exception path (handled below).
    if p.code == 124 {
        ctx.log("Pi session timed out");
        escalation::note_timeout(ctx, &goal, NOTE_LIMIT);
        gitops::drop_branch(ctx, &branch, "noop", "Pi session timed out.", "sleeping");
        return;
    }

    // Redact at the single source the commit/PR/history/log/heartbeat all derive from.
    let raw_summary = ctx.redact(&pi::final_text(&p.stdout));
    let mut summary = if raw_summary.is_empty() {
        "(no summary returned)".to_string()
    } else {
        raw_summary
    };
    let (clean, dev) = escalation::split_item_status(&summary);
    summary = clean;
    item_deviated = item_deviated || dev;
    ctx.log(&format!("Pi rc={}: {}", p.code, char_slice(&summary, 200)));

    // CRASH-NOT-NOOP: an extension/startup load failure is the AGENT being unrunnable, not a no-op.
    let stderr = p.stderr.clone();
    static LOAD_FAIL_RE: OnceLock<Regex> = OnceLock::new();
    let load_fail = LOAD_FAIL_RE
        .get_or_init(|| Regex::new(r"Failed to load extension|Cannot find module").unwrap())
        .is_match(&stderr);
    if p.code != 0 && p.stdout.trim().is_empty() && load_fail {
        // why = _redact((p.stderr or "").strip())[-300:]
        let redacted = ctx.redact(p.stderr.trim());
        let why = tail_chars(&redacted, 300);
        ctx.log(&format!(
            "Pi UNRUNNABLE (extension/startup load error — agent never started; not a model no-op): {why}"
        ));
        gitops::drop_branch(
            ctx,
            &branch,
            "preflight",
            &format!(
                "Pi agent is UNRUNNABLE (extension/startup load error — not a model no-op): {why}"
            ),
            "error",
        );
        ctx.heartbeat(json!({"reason": "agent_unrunnable"})); // distinguishing diagnostic
        return;
    }

    // OBSERVABILITY: a nonzero exit with no stdout that ISN'T a recognized load-failure (e.g. a refused
    // batch spawn -> rc=-1 "batch file arguments are invalid", or any exec failure) used to fall
    // silently through to the "made no changes" no-op below, hiding the real cause for hours. Surface
    // the captured stderr (redacted) so the operator sees WHY pi produced nothing. Additive log only —
    // control flow is unchanged; the noop drop still runs.
    if p.code != 0 && p.stdout.trim().is_empty() && !load_fail {
        let why = tail_chars(&ctx.redact(p.stderr.trim()), 300);
        ctx.log(&format!(
            "Pi exited rc={} with no output (likely a spawn/exec failure, not a model no-op): {why}",
            p.code
        ));
    }

    if !gitops::tree_dirty(ctx) && gitops::head_sha(ctx) == base {
        let agent_untracked = gitops::untracked_non_ignored_files(ctx);
        if agent_untracked.is_empty() {
            let narrated_noop = gates::narrated_without_writing(&summary);
            if narrated_noop {
                ctx.log(
                    "WARNING: Pi narrated a change but wrote nothing to a clean tree — the model likely \
hallucinated its file edits; counting as a no-op",
                );
                summary = format!("[narrated-but-unwritten] {summary}");
            } else {
                ctx.log("Pi made no changes — dropping branch");
            }
            if narrated_noop {
                escalation::note_narrated_noop(ctx, &goal, NOTE_LIMIT);
            } else {
                escalation::note_noop(ctx, &goal, NOTE_LIMIT);
            }
            gitops::drop_branch(ctx, &branch, "noop", &summary, "sleeping");
            return;
        }
        ctx.log(&format!(
            "Pi wrote {} new untracked file(s) — not a noop (proceeding to gate)",
            agent_untracked.len()
        ));
    }

    let tests: Value;
    if ctx.beautify {
        // docs-only pass — there is nothing to test.
        ctx.log("beautify: gate skipped (docs-only)");
        tests = Value::Null;
    } else {
        ctx.heartbeat(json!({"phase": "test", "last_summary": summary}));
        let (green, t, tail) = gates::run_gate(ctx);
        tests = t;
        ctx.heartbeat(json!({"tests": tests}));
        ctx.log(&format!(
            "gate: {} {}",
            if green { "GREEN" } else { "RED" },
            py_dict_str(&tests)
        ));
        if !green {
            ctx.log(&format!(
                "gate tail: {}",
                ctx.redact(&tail_chars(&tail, 400))
            ));
            let failed = tests.get("failed").and_then(Value::as_i64).unwrap_or(0);
            escalation::note_revert(
                ctx,
                &goal,
                &format!(
                    "the change FAILED the test gate ({failed} test(s) failed) — fix the failing \
tests by correcting the implementation"
                ),
                NOTE_LIMIT,
            );
            gitops::drop_branch(
                ctx,
                &branch,
                "reverted",
                &format!("Reverted — tests failed ({failed} failed). {summary}"),
                "sleeping",
            );
            return;
        }
        // Anti-gaming: stage first so NEW UNTRACKED test files are in the diff, then diff vs base.
        gitops::git_add_all(ctx);
        let diff = ctx.git(&["diff", "--cached", &base_branch], 120).stdout;
        let gamed = gates::anti_gaming_reason(ctx, &base_tests, &tests, &diff);
        if let Some(gamed) = gamed {
            ctx.log(&format!("anti-gaming: {gamed} — reverting"));
            escalation::note_revert(
                ctx,
                &goal,
                &format!(
                    "the gate was GAMED ({gamed}) and reverted — make the REAL tests pass; do not \
skip, xfail, delete, or weaken any test"
                ),
                NOTE_LIMIT,
            );
            gitops::drop_branch(
                ctx,
                &branch,
                "reverted",
                &format!("Reverted — anti-gaming: {gamed}. {summary}"),
                "sleeping",
            );
            return;
        }

        // Cross-repo correlated test gate.
        let mut xrec = json!({});
        let xres = gates::run_cross_repo_gates(ctx, &mut xrec);
        if xres.get("ok").and_then(Value::as_bool) != Some(true) {
            let failed = xres
                .get("failed_repo")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let ftail = xres
                .get("results")
                .and_then(|r| r.get(&failed))
                .and_then(|d| d.get("tail"))
                .and_then(Value::as_str)
                .map(|s| char_slice(s, 300))
                .unwrap_or_default();
            ctx.log(&format!(
                "cross-repo gate RED on dep '{failed}' — reverting: {ftail}"
            ));
            gitops::drop_branch(
                ctx,
                &branch,
                "reverted",
                &format!("Reverted — cross-repo gate RED on dep '{failed}'. {summary}"),
                "sleeping",
            );
            // record the cross-repo outcome in history even on a revert.
            let extra = json!({"cross_repo_gates": xrec.get("cross_repo_gates").cloned().unwrap_or(json!({}))});
            ctx.record_history("reverted", Some(&branch), &summary, Some(&extra));
            return;
        }
        if let Some(crg) = xrec.get("cross_repo_gates").and_then(Value::as_object) {
            if !crg.is_empty() {
                let keys: Vec<&String> = crg.keys().collect();
                ctx.log(&format!("cross-repo gates GREEN for deps: {keys:?}"));
            }
        }

        // Per-repo EVAL_CMD eval gate.
        if !ctx.beautify && !gates::eval_cmd(ctx, &ctx.name.clone()).is_empty() {
            let evr = gates::run_eval_gate(ctx, base_eval);
            let ev_score = evr.get("score").cloned().unwrap_or(Value::Null);
            ctx.heartbeat(json!({"eval_score": ev_score}));
            if evr.get("ok").and_then(Value::as_bool) != Some(true) {
                let ev_reason = evr
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("eval score dropped")
                    .to_string();
                ctx.log(&format!("eval gate RED: {ev_reason} — reverting"));
                gitops::drop_branch(
                    ctx,
                    &branch,
                    "reverted",
                    &format!("Reverted — eval gate: {ev_reason}. {summary}"),
                    "sleeping",
                );
                return;
            }
            if let Some(s) = evr.get("score").and_then(Value::as_f64) {
                ctx.log(&format!(
                    "eval gate GREEN: score {} (baseline {})",
                    fmt_float(s),
                    match base_eval {
                        Some(b) => fmt_float(b),
                        None => "None".to_string(),
                    }
                ));
            }
        }
    }

    // commit anything Pi left uncommitted.
    ctx.heartbeat(json!({"phase": "commit"}));
    let add = gitops::git_add_all(ctx);
    if add.code != 0 {
        let err: String = add.stderr.trim().chars().take(200).collect();
        ctx.log(&format!("git add -A failed: {err}"));
        gitops::abort_branch(ctx, &branch);
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "commit",
            "last_summary": format!(
                "git add failed — the agent's change could not be staged ({err}). If an untracked \
        Windows reserved-name file (e.g. `nul`) is in the tree, add it to .git/info/exclude."
            ),
        }));
        ctx.record_history("error", Some(&branch), &summary, None);
        return;
    }
    if ctx.git(&["diff", "--cached", "--quiet"], 120).code != 0 {
        let title = if ctx.beautify {
            "beautify repo".to_string()
        } else {
            ship::pr_title(ctx, if item_deviated { "" } else { &goal }, &summary)
        };
        let prefix = if ctx.beautify { "docs" } else { "rsi" };
        ctx.git(
            &["commit", "-m", &format!("{prefix}: {title}\n\n{summary}")],
            120,
        );
    }
    let rl = ctx.git(
        &["rev-list", "--count", &format!("{base_branch}..HEAD")],
        120,
    );
    if rl.code != 0 {
        let detail: String = rl.stderr.trim().chars().take(160).collect();
        ctx.log(&format!(
            "rev-list failed: {detail} — keeping {branch} for inspection"
        ));
        ctx.git(&["checkout", &base_branch], 120);
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "commit",
            "last_summary": format!(
                "Gate passed but commit count couldn't be verified; {branch} kept. {summary}"
            ),
        }));
        return;
    }
    if rl.stdout.trim() == "0" {
        ctx.log("no commits ahead after gate — dropping branch");
        gitops::drop_branch(ctx, &branch, "noop", &summary, "sleeping");
        return;
    }

    // Catch a DEVIATING agent that reports ITEM-STATUS: done while shipping UNRELATED work.
    if !ctx.beautify && !ctx.solomon && !item_deviated {
        let changed = ctx
            .git(
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    &format!("{base_branch}..{branch}"),
                ],
                120,
            )
            .stdout;
        if escalation::deviated_from_named_files(ctx, &goal, &changed) {
            ctx.log(&format!(
                "DEVIATION: the item names file(s) the committed diff never touched — agent shipped \
unrelated work; not ticking '{}'",
                char_slice(&goal, 60)
            ));
            item_deviated = true;
        } else if gates::item_demands_tests(&goal) {
            let full_diff = ctx
                .git(&["diff", &format!("{base_branch}..{branch}")], 120)
                .stdout;
            if gates::added_test_defs(&full_diff).is_empty() {
                ctx.log(&format!(
                    "DEVIATION: item asks to add tests but the committed diff added no test definition — \
not ticking '{}'",
                    char_slice(&goal, 60)
                ));
                item_deviated = true;
            }
        }
    }

    // LEAK GUARD (PUBLIC repos only).
    if gitops::repo_is_public(ctx, &ctx.name.clone()) {
        let full_diff = ctx
            .git(&["diff", &format!("{base_branch}..{branch}")], 120)
            .stdout;
        let leak = gates::leak_in_diff(ctx, &full_diff);
        if !leak.is_empty() {
            ctx.log(&format!(
                "LEAK GUARD: {leak} — reverting (won't push private/secret data to a public repo)"
            ));
            gitops::drop_branch(
                ctx,
                &branch,
                "reverted",
                &format!(
                    "Reverted — leak guard: {leak}. The committed change would push private operator \
data or a secret to the PUBLIC repo; fix the change to exclude it."
                ),
                "sleeping",
            );
            return;
        }
    }

    // Adversarial REVIEW / JUDGE phase (the SECOND gate).
    if ctx.review_enabled
        && !ctx.beautify
        && !ctx.solomon
        && phases::run_review_phase(ctx, &branch, &goal, &summary) == "reject"
    {
        return; // branch already reverted inside run_review_phase
    }

    // Visual E2E review (best-effort; may HARD-gate when visual_gate is on).
    if ctx.visual_review_enabled
        && is_truthy_obj(&ctx.sandbox_config)
        && !ctx.beautify
        && !ctx.solomon
    {
        ctx.heartbeat(json!({"phase": "review", "last_summary": summary}));
        ctx.log("visual review: starting (sandbox + capture + vision agent)...");
        // visual::run_visual_gate reads sandbox_config/vision_model off Ctx + iteration_summary off
        // the heartbeat last_summary just set, and assigns LAST_VISUAL_FEEDBACK itself (ok -> fb,
        // else ""), mirroring the run_improver.py call site.
        let vr = visual::run_visual_gate(ctx);
        if vr.get("ok").and_then(Value::as_bool) == Some(true) {
            let findings: Vec<Value> = vr
                .get("findings")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let fb = vr.get("feedback").and_then(Value::as_str).unwrap_or("");
            let n_crit = findings
                .iter()
                .filter(|f| f.get("severity").and_then(Value::as_str) == Some("critical"))
                .count();
            let n_warn = findings
                .iter()
                .filter(|f| f.get("severity").and_then(Value::as_str) == Some("warning"))
                .count();
            ctx.log(&format!(
                "visual review: complete — {} findings ({n_crit} critical, {n_warn} warning); feedback {}",
                findings.len(),
                if fb.is_empty() { "empty" } else { "set" }
            ));
            // _hb["visual_review"] = { ts, summary, findings, screenshot_count }
            let screenshot_count = vr
                .get("screenshots")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            hb_set_visual_review(
                ctx,
                json!({
                    "ts": vr.get("ts").cloned().unwrap_or(Value::Null),
                    "summary": vr.get("summary").and_then(Value::as_str).unwrap_or(""),
                    "findings": findings,
                    "screenshot_count": screenshot_count,
                }),
            );
            // Visual review HARD gate.
            if gates::visual_gate_enabled(ctx, &ctx.name.clone()) {
                if let Some(vreason) = gates::visual_gate_reason(&vr) {
                    ctx.log(&format!(
                        "visual gate RED (visual_gate=on): {vreason} — reverting"
                    ));
                    gitops::drop_branch(
                        ctx,
                        &branch,
                        "reverted",
                        &format!("Reverted — visual gate: {vreason}. {summary}"),
                        "sleeping",
                    );
                    return;
                }
            }
        } else {
            let err = vr.get("error").and_then(Value::as_str).unwrap_or("?");
            ctx.log(&format!(
                "visual review: failed — {}; continuing to ship",
                char_slice(err, 200)
            ));
            // run_visual_gate already set LAST_VISUAL_FEEDBACK = "" on the not-ok path.
        }
    }

    // honor an operator Stop that arrived during the (possibly long) iteration.
    if ctx.stop_path.exists() {
        ctx.log("stop requested during iteration — committed locally, skipping PR");
        ctx.git(&["checkout", &base_branch], 120);
        ctx.heartbeat(json!({
            "status": "stopped",
            "phase": "sleep",
            "last_pr": {
                "number": Value::Null,
                "url": Value::Null,
                "branch": branch,
                "state": "local (stopped before PR)",
            },
            "last_summary": summary,
        }));
        ctx.record_history("stopped", Some(&branch), &summary, None);
        return;
    }

    let title = if ctx.beautify {
        "beautify repo".to_string()
    } else {
        ship::pr_title(ctx, if item_deviated { "" } else { &goal }, &summary)
    };
    let pr = ship::ship(ctx, &branch, &title, &summary, &tests);
    ctx.git(&["checkout", &base_branch], 120);
    // Branch hygiene: delete the iteration branch unless it's the SOLE copy of the work.
    let pr_state_lower = pr
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if ctx.ship != "local" && !pr_state_lower.starts_with("local") {
        ctx.git(&["branch", "-D", &branch], 120);
    }
    if gitops::tree_dirty(ctx) {
        ctx.git(&["reset", "--hard"], 120);
    }
    // Advance the backlog ONLY on a real LANDED ship of the NAMED item.
    let landed = ship::ship_succeeded(&pr);
    if !ctx.beautify && !ctx.solomon && landed {
        if item_deviated {
            escalation::note_deviation(ctx, &goal, NOTE_LIMIT);
        } else {
            backlog::mark_backlog_done(ctx, &goal);
            escalation::clear_failure_state(ctx, &goal);
        }
    }
    ctx.heartbeat(json!({
        "status": "sleeping",
        "phase": "sleep",
        "last_pr": pr,
        "last_summary": summary,
    }));
    let ship_mode = ctx.ship.clone();
    ctx.record_history(
        &ship::ship_outcome(&pr, &ship_mode),
        Some(&branch),
        &summary,
        None,
    );
}

// --------------------------------------------------------------------------- #
// ideate_phase + ideate (run_improver ~3191-3263) — not provided by a leaf module
// --------------------------------------------------------------------------- #

/// run_improver.ideate_phase (~3248-3263): pipeline-phase wrapper for ideate(), run BEFORE plan at
/// the top of an iteration when the backlog is thin. No-op when disabled or when the backlog already
/// has >=5 actionable items (a refill valve, not a firehose). Best-effort: never wedges the loop
/// (ideate() here cannot panic — run_pi returns a RunOut — so the Python `except Exception` branch is
/// structurally unreachable; the same "continuing to plan/implement" outcome holds).
fn ideate_phase(ctx: &mut Ctx) {
    if !ctx.ideate_enabled {
        return;
    }
    if backlog::unchecked_backlog_count(ctx) >= 5 {
        return; // refill valve, not a firehose — only ideate when the menu is thin
    }
    ctx.heartbeat(json!({"phase": "ideate"}));
    let rc = ideate(ctx);
    ctx.log(&format!(
        "ideate phase: {} (rc={rc})",
        if rc == 0 { "ok" } else { "no fresh ideas" }
    ));
}

/// run_improver.ideate (~3191-3245): one divergent pass — pi proposes ambitious, leverage-ranked,
/// tier-tagged improvements; the runner PREPENDS the novel ones (highest-leverage first) to the
/// backlog as `- [ ] [tier] ...`. Returns 0 on a successful prepend, else 5 (no parseable / only
/// duplicate ideas, or a write error). No git, no gate (the greedy loop runs them later).
///
/// DEVIATION: the Python `ideate()` also `print(json.dumps(...))`s one status line (it doubles as the
/// `--ideate` one-shot CLI entry). In-loop (the only caller here, `ideate_phase`) that stdout line is
/// not consumed; it is dropped to keep the loop's stdout clean. The return code (0/5) — the only
/// value the in-loop caller reads — is byte-identical.
fn ideate(ctx: &mut Ctx) -> i64 {
    let task = ideate_task(ctx);
    let p = pi::run_pi(
        ctx,
        &task,
        pi::TIMEOUT_PHASE_600,
        Some(&ctx.ideate_md.clone()),
    );
    // A timeout returns rc=124 with (usually) empty stdout — final_text is "" -> no ideas -> rc 5,
    // the same observable outcome as the Python `except TimeoutExpired -> return 5` branch.
    let raw = pi::final_text(&p.stdout);
    let ideas = parse_ideas(&raw);
    if ideas.is_empty() {
        ctx.log(&format!(
            "ideate: no parseable ideas. Raw agent output (first 800 chars):\n{}",
            char_slice(&raw, 800)
        ));
        return 5;
    }
    let existing = if ctx.backlog.exists() {
        std::fs::read_to_string(&ctx.backlog).unwrap_or_else(|_| "# backlog\n".to_string())
    } else {
        "# backlog\n".to_string()
    };

    // NOVELTY SCORING: drop ideas near-duplicating the backlog / recent history / lessons.
    let mut corpus: Vec<String> = existing
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| l.starts_with("- "))
        .collect();
    corpus.extend(recent_history_summaries(ctx, 30));
    corpus.extend(
        read_lessons(ctx)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with("- ")),
    );

    let mut fresh: Vec<(i64, String, String)> = Vec::new();
    let mut dropped = 0i64;
    for (lev, tier, idea) in ideas {
        if is_novel(&idea, &corpus, 0.6) {
            corpus.push(idea.clone()); // so two near-identical candidates in one batch also dedupe
            fresh.push((lev, tier, idea));
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        ctx.log(&format!(
            "ideate: dropped {dropped} near-duplicate idea(s) (novelty filter); {} fresh",
            fresh.len()
        ));
    }
    if fresh.is_empty() {
        ctx.log("ideate: every candidate was a near-duplicate of the backlog/history/lessons — nothing fresh");
        return 5;
    }
    let new_lines: Vec<String> = fresh
        .iter()
        .map(|(_lev, tier, idea)| format!("- [ ] [{tier}] {idea}"))
        .collect();
    // prepend above the existing menu, after a header line if present.
    let lines: Vec<&str> = existing.lines().collect();
    let head = if lines
        .first()
        .map(|l| l.trim_start().starts_with('#'))
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    let mut merged: Vec<String> = Vec::new();
    merged.extend(lines[..head].iter().map(|s| s.to_string()));
    if head > 0 {
        merged.push(String::new());
    }
    merged.extend(new_lines.iter().cloned());
    merged.extend(lines[head..].iter().map(|s| s.to_string()));
    // "\n".join(merged).rstrip() + "\n"
    let body = format!("{}\n", merged.join("\n").trim_end());
    if let Some(parent) = ctx.backlog.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&ctx.backlog, body).is_err() {
        return 5;
    }
    0
}

/// run_improver._ideate_task (~3168-3188): the ideate lane's pi task text. The `ideate_research`
/// branch PERMITS (not requires) web/docs lookup when THIS repo opted in via repos.json.
fn ideate_task(ctx: &Ctx) -> String {
    let goal_line = if !ctx.goal.is_empty() {
        format!(
            "\n\nNORTH-STAR GOAL (rank every idea by how much it advances THIS):\n{}\n",
            ctx.goal
        )
    } else {
        "\n\n(No north-star goal set — propose the highest-leverage improvements toward making \
this project excellent at what it's for.)\n"
            .to_string()
    };
    let research_line = if ideate_research_enabled(ctx, &ctx.name) {
        "\n\nEXTERNAL RESEARCH ALLOWED (ideate_research is on): you MAY look up new ideas on the \
internet — search the web, read docs, find papers or techniques the project doesn't yet use — to \
escape the local minimum and propose genuinely novel, high-leverage moves. This is OPTIONAL, not \
required; ground every idea in the real code you read AND the external research. Your output is \
still REVIEWABLE backlog items — the runner sorts and prepends them; you NEVER edit the backlog \
file yourself (menu curation stays human-owned).\n"
    } else {
        ""
    };
    format!(
        "Read this repository and propose its next batch of ambitious, high-leverage improvements \
per ideate.md. Output ONLY the idea lines.{research_line}{goal_line}"
    )
}

/// run_improver._ideate_research_enabled (~3157-3165): True iff THIS repo set `ideate_research:
/// true` in repos.json (read fresh). False when absent.
fn ideate_research_enabled(ctx: &Ctx, name: &str) -> bool {
    value_truthy(
        gitops::repo_row(ctx, name)
            .get("ideate_research")
            .unwrap_or(&Value::Null),
    )
}

/// run_improver._parse_ideas (~3124-3154): parse the ideate lane's idea lines into
/// `(leverage, tier, idea)` tuples, highest-leverage first. Strict tier-tagged form first; falls
/// back to plain idea lines (tier=feature, leverage=3) with noise guards. Pure.
fn parse_ideas(text: &str) -> Vec<(i64, String, String)> {
    let strict = strict_idea_re();
    static PLAIN_STRIP: OnceLock<Regex> = OnceLock::new();
    let plain_strip = PLAIN_STRIP.get_or_init(|| Regex::new(r"^[\s\-*•·\d.)>]+").unwrap());
    static META_RE: OnceLock<Regex> = OnceLock::new();
    let meta_re = META_RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(idea lines|here|below|based on|i|no|note|first|second|third|next|the following|these|this (is|repo|project)|propose)\b",
        )
        .unwrap()
    });

    let mut tiered: Vec<(i64, String, String)> = Vec::new();
    let mut plain: Vec<(i64, String, String)> = Vec::new();
    for ln in text.lines() {
        let s = ln.trim();
        if let Some(caps) = strict.captures(s) {
            // strict.captures already requires a match at position 0 (the pattern is ^-anchored).
            let idea_raw = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            if idea_raw.trim().chars().count() > 8 {
                let lev = match caps.get(2).map(|m| m.as_str()) {
                    Some(d) if !d.is_empty() => match d.parse::<i64>() {
                        Ok(v) => v.clamp(1, 5),
                        // a huge all-ASCII-digit run overflows i64 — Python's arbitrary-precision
                        // int() would clamp it to 5, not fall back to the default 3. Any OTHER parse
                        // failure (a Unicode `\d` digit that Rust's i64::parse rejects but Python's
                        // int() would accept) keeps the prior default rather than wrongly saturating.
                        Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => 5,
                        Err(_) => 3,
                    },
                    _ => 3,
                };
                let tier = caps
                    .get(1)
                    .map(|m| m.as_str())
                    .unwrap_or("feature")
                    .to_lowercase();
                // m.group(3).strip().rstrip("`").strip()
                let idea = idea_raw.trim().trim_end_matches('`').trim().to_string();
                tiered.push((lev, tier, idea));
                continue;
            }
        }
        // fallback: a plain idea line.
        let stripped = plain_strip.replace(s, "");
        let body = stripped.trim().trim_end_matches('`').trim().to_string();
        let low = body.to_lowercase();
        if body.chars().count() > 30
            && body.contains(' ')
            && !low.starts_with('#')
            && !low.starts_with("[chore]")
            && !body.ends_with(':')
            && !meta_re.is_match(&body)
        {
            plain.push((3, "feature".to_string(), body));
        }
    }
    let mut ideas = if !tiered.is_empty() { tiered } else { plain };
    // ideas.sort(key=lambda t: -t[0]) — stable descending by leverage (Python sort is stable).
    ideas.sort_by_key(|idea| std::cmp::Reverse(idea.0));
    ideas
}

/// The strict tier-line regex of _parse_ideas (~3138-3139), `re.I`, anchored at the start:
/// optional bullet/number/markdown, then a `(feature|refactor|architecture)` tag (chore excluded),
/// an optional leverage int, then the idea body. Groups: 1=tier, 2=leverage(optional), 3=idea.
fn strict_idea_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"^[\s\-*\d.)#>]*\**\[?\s*(feature|refactor|architecture)\s*\]?\**\s*[|:\-–—]*\s*(\d+)?\s*[|:\-–—]*\s*(.+?)\s*$",
        )
        .case_insensitive(true)
        .build()
        .expect("strict idea regex compiles")
    })
}

/// run_improver._recent_history_summaries (~3101-3120): the last `limit` history.jsonl iteration
/// summaries (oldest-to-newest within the window), skipping blank/unparseable lines and empty
/// summaries. [] on OSError.
fn recent_history_summaries(ctx: &Ctx, limit: usize) -> Vec<String> {
    let path = ctx.runtime.join("history.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    // lines[-limit:]
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(limit);
    let mut out = Vec::new();
    for line in &all[start..] {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let s = rec
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !s.is_empty() {
            out.push(s);
        }
    }
    out
}

// --------------------------------------------------------------------------- #
// _solomon_task (run_improver ~3342-3363)
// --------------------------------------------------------------------------- #

/// run_improver._solomon_task (~3342-3363): build the supervisor fix-session prompt from the recent
/// (failing) iteration history (last 6 records). Names each outcome's status/failed-count/summary[:80].
fn solomon_task(ctx: &Ctx) -> String {
    let path = ctx.runtime.join("history.jsonl");
    let mut hist: Vec<Value> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(&path) {
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(6);
        for line in &all[start..] {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                hist.push(v);
            }
        }
    }
    let parts: Vec<String> = hist
        .iter()
        .map(|h| {
            // f"{status} ({failed} failed): {summary[:80]}"
            let status = match h.get("status") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => "None".to_string(),
                Some(other) => other.to_string(),
            };
            // (h.get('tests') or {}).get('failed', '?')
            let failed = match h.get("tests") {
                Some(Value::Object(t)) => match t.get("failed") {
                    Some(v) => py_scalar_str(v),
                    None => "?".to_string(),
                },
                _ => "?".to_string(),
            };
            let summary_full = h.get("summary").and_then(Value::as_str).unwrap_or("");
            let summary = char_slice(summary_full, 80);
            format!("{status} ({failed} failed): {summary}")
        })
        .collect();
    let outcomes = if parts.is_empty() {
        "no recorded history".to_string()
    } else {
        parts.join("; ")
    };
    format!(
        "You are Solomon, supervising this repository's RSI loop, which keeps FAILING its test \
gate. Recent outcomes: {outcomes}. Read the failing test(s) and the code they guard, find the \
ROOT CAUSE (a flaky or incorrect test, an unmet dependency, or a wrong instruction in the agent \
contract), and make the SMALLEST fix — to the one offending test or the code it covers — so the \
gate goes green. Do NOT run git or gh. If the cause is genuinely ambiguous, write a 2-4 sentence \
diagnosis and make no code change."
    )
}

// --------------------------------------------------------------------------- #
// lesson-dedup helpers (private copies, identical to phases.rs) — used by ideate
// --------------------------------------------------------------------------- #

/// run_improver._read_lessons (~3091-3098): the accumulated LESSONS.md text; "" when absent/unreadable.
fn read_lessons(ctx: &Ctx) -> String {
    if ctx.lessons.exists() {
        std::fs::read_to_string(&ctx.lessons).unwrap_or_default()
    } else {
        String::new()
    }
}

/// run_improver._STOPWORDS (~3046-3048).
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "for", "to", "of", "in", "on", "at", "by", "with",
    "from", "into", "as", "is", "are", "be", "it", "this", "that", "these", "those", "add", "adds",
    "added", "use", "uses", "using", "make", "makes", "made", "into", "via", "per", "its", "it's",
    "not", "no", "than", "then", "so", "we", "i",
];

/// run_improver._tokenize (~3051-3054): lowercase `[a-z0-9]+` tokens, dropping stopwords + len<=2.
fn tokenize(text: &str) -> HashSet<String> {
    // r"[a-z0-9]+" over a lowercased string == maximal runs of ascii-alphanumerics; stdlib split gives
    // the same tokens with no regex compile (this runs per corpus item inside is_novel's O(N) loop).
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_string)
        .filter(|w| w.chars().count() > 2 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// run_improver._is_novel (~3066-3088): True iff `idea` is NOT a near-duplicate of any `corpus`
/// entry. Empty token set -> always novel. Jaccard >= threshold OR containment >= threshold+0.15 ->
/// not novel.
fn is_novel(idea: &str, corpus: &[String], threshold: f64) -> bool {
    let toks = tokenize(idea);
    if toks.is_empty() {
        return true;
    }
    for other in corpus {
        let ot = tokenize(other);
        if ot.is_empty() {
            continue;
        }
        let inter = toks.intersection(&ot).count();
        if inter == 0 {
            continue;
        }
        let union = toks.union(&ot).count();
        if (inter as f64) / (union as f64) >= threshold {
            return false;
        }
        let min_len = toks.len().min(ot.len());
        if (inter as f64) / (min_len as f64) >= threshold + 0.15 {
            return false;
        }
    }
    true
}

// --------------------------------------------------------------------------- #
// small helpers
// --------------------------------------------------------------------------- #

/// `_hb["iteration"] += 1` — bump the heartbeat iteration counter in place (NOT a heartbeat() call,
/// so no freeze logic / updated_at rewrite). Treats a missing/non-int counter as 0.
fn increment_iteration(ctx: &mut Ctx) {
    if let Value::Object(hb) = &mut ctx.hb {
        let n = hb.get("iteration").and_then(Value::as_i64).unwrap_or(0) + 1;
        hb.insert("iteration".to_string(), json!(n));
    }
}

/// `_hb["visual_review"] = {...}` — a bare heartbeat-dict field assignment (not a heartbeat() call).
fn hb_set_visual_review(ctx: &mut Ctx, value: Value) {
    if let Value::Object(hb) = &mut ctx.hb {
        hb.insert("visual_review".to_string(), value);
    }
}

/// Python `s[:n]` by code points (NOT bytes).
fn char_slice(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `s[-n:]` by code points.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
}

/// Python truthiness for an optional JSON value (null/false/0/""/[]/{} -> false).
fn value_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `if d:` truthiness for a dict-shaped value (a non-empty object). A non-object is falsy.
fn is_truthy_obj(v: &Value) -> bool {
    matches!(v, Value::Object(o) if !o.is_empty())
}

/// Python `str(x)` of a tests dict / value rendered into a log or summary line: a JSON object is
/// rendered the way Python `str(dict)` would (single-quoted keys, `True`/`False`/`None`), so a log
/// like `gate: RED {'passed': 0, ...}` matches the source. Used where the source does f"{tests}".
fn py_dict_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(py_dict_str).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(o) => {
            let inner: Vec<String> = o
                .iter()
                .map(|(k, val)| {
                    format!(
                        "'{}': {}",
                        k.replace('\\', "\\\\").replace('\'', "\\'"),
                        py_dict_str(val)
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// Python `str(x)` of a JSON SCALAR (no quoting) — for the solomon-task `failed` count, where the
/// source inserts the value into an f-string directly (a missing key defaults to the literal '?').
fn py_scalar_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Format a float the way Python's f-string `{x}` renders an eval score: integral floats keep `.0`.
fn fmt_float(x: f64) -> String {
    if x == x.trunc() && x.is_finite() {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

// --------------------------------------------------------------------------- #
// tests — pure helpers introduced in this module (the ideate/solomon parsers +
// the string-semantics shims). The IO-bound state machine itself is exercised by
// the loop's integration tests; here we pin the byte-exact parser behavior.
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        // Per-call unique runtime dir: tests run in parallel by default, and several of them write /
        // remove the SAME runtime files (history.jsonl, backlog.md). A shared per-pid dir races (one
        // test's remove_file clobbers another's just-written history). A unique dir per ctx() isolates
        // them without changing any assertion.
        let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut c = Ctx::configure("C:/nonexistent/repo", "testrepo", "ollama-cloud", None);
        c.runtime =
            std::env::temp_dir().join(format!("solomon_iter_test_{}_{}", std::process::id(), uniq));
        c.backlog = c.runtime.join("backlog.md");
        c.lessons = c.runtime.join("LESSONS.md");
        c
    }

    // ---- parse_ideas: strict tier form, leverage clamp, sort, fallback ----
    #[test]
    fn parse_ideas_strict_tier_and_sort() {
        let text = "\
- [feature] 5 add OAuth login support to the dashboard
- [refactor] 2 tidy the gate parsing module into one file
[architecture] 4 | split the runner into a service layer";
        let ideas = parse_ideas(text);
        assert_eq!(ideas.len(), 3);
        // sorted by leverage DESC: 5, 4, 2
        assert_eq!(ideas[0].0, 5);
        assert_eq!(ideas[0].1, "feature");
        assert!(ideas[0].2.starts_with("add OAuth login"));
        assert_eq!(ideas[1].0, 4);
        assert_eq!(ideas[1].1, "architecture");
        assert_eq!(ideas[2].0, 2);
        assert_eq!(ideas[2].1, "refactor");
    }

    #[test]
    fn parse_ideas_leverage_default_and_clamp() {
        // no leverage int -> default 3; >5 clamps to 5.
        let text = "- [feature] add a fully novel widget for the user dashboard\n[refactor] 9 collapse the helper modules into one";
        let ideas = parse_ideas(text);
        // default-3 feature and clamped-5 refactor; sorted desc -> refactor(5) first.
        assert_eq!(ideas[0].0, 5);
        assert_eq!(ideas[0].1, "refactor");
        assert_eq!(ideas[1].0, 3);
        assert_eq!(ideas[1].1, "feature");
    }

    #[test]
    fn parse_ideas_leverage_overflow_saturates_to_five() {
        // a leverage int too large for i64 overflows parse() — Python's int() would clamp the huge
        // value to 5, so it must saturate to 5, not fall back to the default 3.
        let text = "[feature] 99999999999999999999999999 add an overflow-resistant cache layer";
        let ideas = parse_ideas(text);
        assert_eq!(ideas.len(), 1);
        assert_eq!(ideas[0].0, 5);
    }

    #[test]
    fn parse_ideas_chore_tag_dropped_and_short_idea_rejected() {
        // chore is NOT in the strict group; a too-short body (<=8 chars) is rejected from strict.
        // Both fall to the plain-line path, which itself rejects short/meta lines.
        let text = "- [chore] bump dep\n- [feature] go";
        let ideas = parse_ideas(text);
        assert!(ideas.is_empty());
    }

    #[test]
    fn parse_ideas_plain_fallback_with_noise_guards() {
        // No tier-tagged line -> plain fallback (tier=feature, lev=3). Meta/preamble + short + ends-':'
        // lines are dropped; a real substantive bullet survives.
        let text = "\
Here are some ideas:
- Improve the dashboard rendering pipeline so charts paint without flicker
- short one
Based on the code, propose the following:
The following:";
        let ideas = parse_ideas(text);
        assert_eq!(ideas.len(), 1);
        assert_eq!(ideas[0].0, 3);
        assert_eq!(ideas[0].1, "feature");
        assert!(ideas[0].2.starts_with("Improve the dashboard"));
    }

    #[test]
    fn parse_ideas_empty_text() {
        assert!(parse_ideas("").is_empty());
        assert!(parse_ideas("\n\n   \n").is_empty());
    }

    // ---- is_novel / tokenize (parity with the source dedup) ----
    #[test]
    fn is_novel_jaccard_and_containment() {
        let corpus = vec!["prefer the standard library over custom code".to_string()];
        assert!(!is_novel(
            "prefer standard library over custom code",
            &corpus,
            0.6
        ));
        assert!(is_novel(
            "document the licensing terms clearly",
            &corpus,
            0.6
        ));
        // empty token idea (all stopwords) -> always novel
        assert!(is_novel("the a an it", &corpus, 0.6));
    }

    #[test]
    fn tokenize_drops_short_and_stopwords() {
        let t = tokenize("The CI gate is RED on at");
        assert!(t.contains("gate"));
        assert!(t.contains("red"));
        assert!(!t.contains("the"));
        assert!(!t.contains("ci")); // len 2
    }

    // ---- solomon_task: no-history default + record formatting ----
    #[test]
    fn solomon_task_no_history_default() {
        let c = ctx();
        let _ = std::fs::remove_file(c.runtime.join("history.jsonl"));
        let t = solomon_task(&c);
        assert!(t.contains("Recent outcomes: no recorded history."));
        assert!(t.starts_with("You are Solomon, supervising this repository's RSI loop"));
    }

    #[test]
    fn solomon_task_formats_recent_records() {
        let c = ctx();
        std::fs::create_dir_all(&c.runtime).unwrap();
        let hist = "\
{\"status\":\"reverted\",\"tests\":{\"failed\":2},\"summary\":\"tried a thing\"}
{\"status\":\"noop\",\"summary\":\"made no changes\"}
";
        std::fs::write(c.runtime.join("history.jsonl"), hist).unwrap();
        let t = solomon_task(&c);
        // failed count from tests dict; missing tests -> '?'
        assert!(t.contains("reverted (2 failed): tried a thing"));
        assert!(t.contains("noop (? failed): made no changes"));
    }

    // ---- ideate_task: research line + goal line gating ----
    #[test]
    fn ideate_task_default_no_goal_no_research() {
        let c = ctx();
        let t = ideate_task(&c);
        assert!(t.contains("Output ONLY the idea lines."));
        assert!(t.contains("(No north-star goal set"));
        assert!(!t.contains("EXTERNAL RESEARCH ALLOWED"));
    }

    #[test]
    fn ideate_task_with_goal() {
        let mut c = ctx();
        c.goal = "ship revenue features".to_string();
        let t = ideate_task(&c);
        assert!(t.contains("NORTH-STAR GOAL (rank every idea by how much it advances THIS):\nship revenue features"));
    }

    // ---- char_slice / tail_chars / py_dict_str ----
    #[test]
    fn char_and_tail_slices() {
        assert_eq!(char_slice("abcdef", 3), "abc");
        assert_eq!(char_slice("ab", 9), "ab");
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 9), "ab");
    }

    #[test]
    fn py_dict_str_renders_python_style() {
        let v = json!({"passed": 5, "failed": 0, "green": true});
        let s = py_dict_str(&v);
        // preserve_order keeps insertion order; Python str(dict) form with True
        assert_eq!(s, "{'passed': 5, 'failed': 0, 'green': True}");
        assert_eq!(py_dict_str(&Value::Null), "None");
        assert_eq!(py_dict_str(&json!("x")), "'x'");
    }

    #[test]
    fn fmt_float_integral_keeps_decimal() {
        assert_eq!(fmt_float(2.0), "2.0");
        assert_eq!(fmt_float(0.85), "0.85");
    }

    // ---- increment_iteration ----
    #[test]
    fn increment_iteration_bumps_counter() {
        let mut c = ctx();
        // configure seeds iteration=0
        assert_eq!(c.hb.get("iteration").and_then(Value::as_i64), Some(0));
        increment_iteration(&mut c);
        assert_eq!(c.hb.get("iteration").and_then(Value::as_i64), Some(1));
        increment_iteration(&mut c);
        assert_eq!(c.hb.get("iteration").and_then(Value::as_i64), Some(2));
    }
}
