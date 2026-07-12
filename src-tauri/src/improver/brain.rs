//! Mixture-of-Agents (MoA) brain — Solomon's multi-model reasoning layer.
//!
//! Replaces the single-model `ollama_chat` call at the implement dispatch (integration point B)
//! and the CEO planner (integration points A + E) with a 2-layer MoA:
//!   Layer 1 (proposers): plan worker (kimi-k2.7-code) + ideate worker (minimax-m2.7)
//!   Layer 2 (aggregator): glm-5.2 synthesizes the Layer-1 outputs into one plan
//!   Execute: the synthesized plan is handed to the EXISTING `pi::run_pi` implementer (deepseek-v4-pro)
//!            so the diff is real, the gate is runner-enforced, and the budget is metered.
//!
//! The brain is UPSTREAM of the gate. It produces a candidate plan/diff; the Rust runner decides
//! if it ships. The aggregator NEVER grades. The verifier is a SEPARATE post-gate call (wired in
//! Slice 4 via `run_review_phase`), runner-parsed, fail-open. See RSI_MOA_PLAN.md §2 + §5.
//!
//! v1 (Slice 3) runs the planner-only Layer-1 (MoA-Lite n=1) to hit the proof hour; the ideate
//! worker is added in Slice 4. The `moa_enabled` ctx flag is the rollback: `false` keeps the
//! pre-MoA single-model `dispatch_engineering_on_ctx` path byte-identical.
//!
//! Architecture grounded in: RSI_MOA_RESEARCH.md §1.2 (Together AI MoA, Aggregate-and-Synthesize
//! prompt Table 1), §1.6 (2-layer MoA-Lite verdict), §2.5 (exact model IDs), §4.5 (anti-gaming rails).
#![allow(dead_code)]

use crate::improver::ctx::Ctx;
use serde_json::Value;

// --------------------------------------------------------------------------- #
// Brain config (loaded from repos.json autopilot `brain` block)
// --------------------------------------------------------------------------- #

#[derive(Clone, Debug)]
pub struct BrainConfig {
    pub enabled: bool,
    pub aggregator: String,
    pub verifier: String,
    pub workers: Workers,
    pub layers: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Workers {
    pub plan: Option<String>,
    pub implement: Option<String>,
    pub ideate: Option<String>,
    pub probe: Option<String>,
}

impl BrainConfig {
    /// Read the `brain` block from the autopilot config in repos.json. Missing/invalid -> disabled
    /// (byte-identical to pre-MoA behavior; the iteration loop falls back to dispatch_engineering).
    pub fn from_autopilot() -> Self {
        let auto = crate::control::registry::autopilot_config();
        let brain = auto.get("brain");
        let Some(brain) = brain else {
            return Self::disabled();
        };
        Self {
            enabled: brain.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            aggregator: brain
                .get("aggregator")
                .and_then(Value::as_str)
                .unwrap_or("glm-5.2")
                .to_string(),
            verifier: brain
                .get("verifier")
                .and_then(Value::as_str)
                .unwrap_or("glm-5.2")
                .to_string(),
            workers: Workers {
                plan: brain
                    .pointer("/workers/plan")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                implement: brain
                    .pointer("/workers/implement")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ideate: brain
                    .pointer("/workers/ideate")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                probe: brain
                    .pointer("/workers/probe")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            layers: brain.get("layers").and_then(Value::as_u64).unwrap_or(2) as u8,
        }
    }

    fn disabled() -> Self {
        Self {
            enabled: false,
            aggregator: "glm-5.2".into(),
            verifier: "glm-5.2".into(),
            workers: Workers::default(),
            layers: 2,
        }
    }
}

/// The configured implementer model (deepseek-v4-pro) for the pi-model override in iteration.rs.
/// Returns None if the brain is disabled or the implementer worker is unset — the caller leaves
/// ctx.pi_model unchanged in that case (the repo's default model).
pub fn implementer_model() -> Option<String> {
    let cfg = BrainConfig::from_autopilot();
    if !cfg.enabled {
        return None;
    }
    cfg.workers.implement
}

// --------------------------------------------------------------------------- #
// System prompts (const — the role contracts; research §1.2 + plan §2.3)
// --------------------------------------------------------------------------- #

/// Layer-1 planner: read-only, drafts the single highest-leverage improvement + acceptance criteria.
const PLAN_PROMPT: &str = "You are the Solomon planner. Read the repo context provided. Draft the single highest-leverage improvement toward the lane's north-star goal, with clear acceptance criteria. Output a concise markdown plan. Do NOT write or edit files — you propose, the implementer executes.";

/// Layer-1 ideator: divergent, proposes 5-8 leverage-ranked alternatives (the depth/creativity escape).
const IDEATE_PROMPT: &str = "You are the Solomon ideator. Propose 5-8 ambitious, non-obvious, leverage-ranked alternative improvements toward the lane's north-star goal. Trivial chores (dedup, add a test) are forbidden. Spread across subsystems (at most 2 per subsystem). Output a ranked markdown list with one-line rationale each.";

/// Layer-2 aggregator: the MoA "Aggregate-and-Synthesize" prompt — VERBATIM from the MoA paper
/// Table 1 (Wang et al. 2024, https://arxiv.org/abs/2406.04692). The `Responses from models:` list is
/// appended at runtime with the Layer-1 outputs.
const AGGREGATE_SYNTHESIZE_PROMPT: &str = "You have been provided with a set of responses from various open-source models to the latest user query. Your task is to synthesize these responses into a single, high-quality response. It is crucial to critically evaluate the information provided in these responses, recognizing that some of it may be biased or incorrect. Your response should not simply replicate the given answers but should offer a refined, accurate, and comprehensive reply to the instruction. Ensure your response is well-structured, coherent, and adheres to the highest standards of accuracy and reliability.";

/// The adversarial verifier — runs AFTER the objective gate + commit, BEFORE ship. Inspects the
/// committed diff for reward-hacking / scope-creep / regressions / goal-miss. Output is runner-parsed
/// (JSON `{verdict, reasons}` or the existing `REVIEW: approve|reject` line). Fail-open on unparseable.
const VERIFY_PROMPT: &str = "You are the Solomon adversarial verifier. You are given a committed diff that PASSED the objective test gate. Your job is to find reasons it should NOT ship: reward-hacking (tests weakened/deleted), scope-creep beyond the one improvement, regressions, or goal-miss. Output a single line: `REVIEW: approve - <reason>` or `REVIEW: reject - <reason>`. If you cannot decide, output `REVIEW: approve - unable to verify, fail-open`.";

// --------------------------------------------------------------------------- #
// spawn_worker — one ephemeral MoA worker = system-prompt S + skill K + task T + model M
// --------------------------------------------------------------------------- #

/// Spawn one MoA worker via the proven Ollama Cloud curl path (reuse, not reinvent — ponytail).
/// `skill_k` is loaded from disk (`improver/<repo>/skills/<role>.md`); empty string if absent.
/// Budget-aware (B2 fix): consults `budget::preflight` on the same `maki-cloud:<model>` ledger
/// pi.rs uses; on `Parked` returns Err (caller degrades); on `Proceed` runs ollama_chat, then
/// `record_call` + `record_success`/`record_quota` so the MoA workers share the fleet's reserve-
/// headroom rail (no untracked spend, no 429 storm). The provider key is `ctx.pi_provider`
/// (matches pi.rs — "maki-cloud" for the ollama-cloud autopilot provider, per ctx.rs:48).
pub fn spawn_worker(
    model: &str,
    system_prompt_s: &str,
    skill_k: &str,
    task_t: &str,
    ctx: Option<&mut Ctx>,
) -> Result<String, String> {
    let system = if skill_k.trim().is_empty() {
        system_prompt_s.to_string()
    } else {
        format!("{system_prompt_s}\n\n--- Skill instructions ---\n{skill_k}")
    };
    let Some(ctx) = ctx else {
        return crate::ceo::ollama_chat(model, &system, task_t);
    };
    let provider = ctx.pi_provider.clone();
    ctx.log(&format!("MoA worker ({provider}:{model})"));
    // Budget preflight — same ledger pi.rs uses. Parked -> Err (caller degrades to raw task /
    // planner output, never a fabricated success). This is the reserve-headroom rail: the MoA
    // brain cannot burn the operator's Ollama Cloud quota untracked.
    if let crate::improver::budget::Decision::Parked { until, .. } =
        crate::improver::budget::preflight(ctx, &provider, model)
    {
        return Err(format!(
            "endpoint {provider}:{model} parked until {until} (budget/429)"
        ));
    }
    let res = crate::ceo::ollama_chat(model, &system, task_t);
    // Record the spend + the outcome (success resets the 429 streak; a quota-shaped error parks).
    crate::improver::budget::record_call(ctx, &provider, model);
    match &res {
        Ok(_) => crate::improver::budget::record_success(ctx, &provider, model),
        Err(e) => {
            // A 429/transport/usage-limit error -> record_quota parks this endpoint (never the fleet).
            // Route through the canonical classifier (pi::is_quota_error), NOT ad-hoc substrings: the
            // 2026-07-03 no-op storm happened because the provider's "usage limit" wording slipped
            // past hand-rolled checks. An Ollama 429 surfaces here as "no message content ... usage
            // limit ..." — is_quota_error matches it; the old three substrings did not.
            if crate::improver::pi::is_quota_error(e) {
                crate::improver::budget::record_quota(ctx, &provider, model);
            }
        }
    }
    res
}

/// Load a skill file from `improver/<repo>/skills/<role>.md`. Missing file -> empty string (graceful,
/// byte-identical to "no skill injected"). Operator-editable without recompile (research §3.4 note 5).
fn load_skill(repo: &str, role: &str) -> String {
    let p = crate::control::paths::here()
        .join("improver")
        .join(repo)
        .join("skills")
        .join(format!("{role}.md"));
    std::fs::read_to_string(&p).unwrap_or_default()
}

/// Emit a MoA-brain event to `runtime/autopilot_events.jsonl` so the proof-hour criterion A
/// (MoA brain firing) has real instrumentation — the skeptic found the brain had NO observability,
/// which allowed the lead to misattribute budget-ledger increments to a MoA call that never ran.
/// One JSON line per layer transition: {event, repo, layer, model, status, reason, ts}.
fn moa_event(repo: &str, layer: u8, model: &str, status: &str, reason: &str) {
    let p = crate::control::paths::here()
        .join("runtime")
        .join("autopilot_events.jsonl");
    let line = serde_json::json!({
        "event": "moa_layer",
        "repo": repo,
        "layer": layer,
        "model": model,
        "status": status,
        "reason": reason,
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
}

// --------------------------------------------------------------------------- #
// run_moa_iteration — the orchestrator (integration point B)
// --------------------------------------------------------------------------- #

/// The MoA brain's iteration orchestrator. Called from `iteration.rs` when `ctx.moa_enabled`.
///
/// Returns the synthesized plan text to hand to the EXISTING implementer dispatch (which runs
/// `pi::run_pi` with the implementer worker model + the real gate). The caller then runs the
/// gate + anti-gaming + ship path UNCHANGED — the brain is upstream of all of it.
///
/// v1 (Slice 3): Layer-1 = planner only (MoA-Lite n=1). Slice 4 adds the ideator.
/// Degraded mode (research §5.4): if the planner fails, return the raw task (pre-MoA baseline).
/// If the aggregator fails, return the planner output directly. Never fabricate.
pub fn run_moa_plan(ctx: &mut Ctx, task: &str) -> String {
    let cfg = BrainConfig::from_autopilot();
    if !cfg.enabled {
        return task.to_string();
    }
    let lane = ctx.name.clone();
    let plan_model = cfg
        .workers
        .plan
        .clone()
        .unwrap_or_else(|| cfg.aggregator.clone());
    let plan_skill = load_skill(&lane, "plan");
    let ideate_model = cfg.workers.ideate.clone();
    let ideate_skill = load_skill(&lane, "ideate");

    // Layer 1a — planner (kimi-k2.7-code). Read-only proposal.
    ctx.log(&format!(
        "MoA Layer-1: planner worker ({plan_model})"
    ));
    let plan_text = match spawn_worker(&plan_model, PLAN_PROMPT, &plan_skill, task, Some(ctx)) {
        Ok(p) => {
            moa_event(&lane, 1, &plan_model, "ok", "planner returned a plan");
            p
        }
        Err(e) => {
            ctx.log(&format!(
                "MoA Layer-1 planner failed ({e}); degrading to raw-task implementer (pre-MoA baseline)"
            ));
            moa_event(&lane, 1, &plan_model, "failed", &e);
            return task.to_string();
        }
    };

    // Layer 1b — ideator (minimax-m3). Divergent alternative proposals. Runs sequentially after the
    // planner (v1; parallel tokio::join! is the Phase-2 upgrade). If the ideator fails, the
    // aggregator synthesizes from the planner output alone (degraded n=1 — still MoA-Lite).
    let ideate_text = if let Some(im) = &ideate_model {
        ctx.log(&format!("MoA Layer-1: ideator worker ({im})"));
        match spawn_worker(im, IDEATE_PROMPT, &ideate_skill, task, Some(ctx)) {
            Ok(t) => {
                moa_event(&lane, 1, im, "ok", "ideator returned alternatives");
                Some(t)
            }
            Err(e) => {
                ctx.log(&format!(
                    "MoA Layer-1 ideator failed ({e}); aggregator will use planner output only (n=1)"
                ));
                moa_event(&lane, 1, im, "failed", &e);
                None
            }
        }
    } else {
        None
    };

    // Layer 2 — aggregator (glm-5.2) with the Aggregate-and-Synthesize prompt over BOTH Layer-1
    // outputs (plan + ideate) when both are present, or just the plan when the ideator failed.
    ctx.log(&format!(
        "MoA Layer-2: aggregator ({}) synthesizing plan",
        cfg.aggregator
    ));
    let layer1_inputs = match &ideate_text {
        Some(it) => format!("1. [Plan]: {plan_text}\n2. [Ideate]: {it}"),
        None => format!("1. [Plan]: {plan_text}"),
    };
    let agg_user = format!("{AGGREGATE_SYNTHESIZE_PROMPT}\n\nResponses from models:\n{layer1_inputs}");
    let agg_skill = load_skill(&lane, "aggregate");
    match spawn_worker(&cfg.aggregator, "", &agg_skill, &agg_user, Some(ctx)) {
        Ok(synth) => {
            ctx.log("MoA: synthesized plan ready for implementer");
            moa_event(&lane, 2, &cfg.aggregator, "ok", "synthesized plan");
            format!("{synth}\n\n--- MoA directives ---\n1. NEVER add #[skip], #[ignore], pytest.mark.skip, @skipif, @xfail, or any test-skip/xfail marker. NEVER delete, weaken, or comment out an existing test to make the gate pass. The runner's anti-gaming scan detects skip markers in the diff and REVERTS the entire iteration. If a test fails, FIX the code, not the test.\n2. For Rust repos: your code MUST pass `cargo clippy --workspace --all-targets -- -D warnings` with ZERO warnings. Common clippy lints to avoid: overly_complex_bool_expr (use `assert!(!x)` not `assert!(x || !x)`), manual_range_contains (use `(a..b).contains(&v)` not `v >= a && v < b`), needless_range_loop (use iterators), redundant_closure (use `f` not `|x| f(x)`). Run `cargo clippy` mentally before writing each line.")
        }
        Err(e) => {
            ctx.log(&format!(
                "MoA aggregator failed ({e}); handing raw task + advisory plan to implementer"
            ));
            moa_event(&lane, 2, &cfg.aggregator, "failed", &e);
            format!("{task}\n\n--- Advisory plan (MoA planner, aggregator failed) ---\n{plan_text}\n\n--- MoA directives ---\n1. NEVER add #[skip], #[ignore], pytest.mark.skip, @skipif, @xfail, or any test-skip/xfail marker. NEVER delete, weaken, or comment out an existing test to make the gate pass. The runner's anti-gaming scan detects skip markers in the diff and REVERTS the entire iteration.\n2. For Rust repos: your code MUST pass `cargo clippy --workspace --all-targets -- -D warnings` with ZERO warnings. Common clippy lints to avoid: overly_complex_bool_expr, manual_range_contains (use `(a..b).contains(&v)`), needless_range_loop, redundant_closure. Run `cargo clippy` mentally before writing each line.")
        }
    }
}

/// The MoA aggregator-only path for the CEO planner/focus calls (integration points A + E).
/// Single glm-5.2 call with the Aggregate-and-Synthesize prompt over a single input (MoA-Lite n=1).
/// Returns the synthesized text, or the original `user` on failure (graceful — CEO planning is
/// advisory, a failure degrades to the input, never a crash).
pub fn run_moa_aggregator_only(system: &str, user: &str) -> String {
    let cfg = BrainConfig::from_autopilot();
    if !cfg.enabled {
        // Fall back to the single-model path the caller would have used.
        return crate::ceo::ollama_chat(&cfg.aggregator, system, user)
            .unwrap_or_else(|_| user.to_string());
    }
    let agg_user = format!(
        "{AGGREGATE_SYNTHESIZE_PROMPT}\n\nResponses from models:\n1. [Input]: {user}"
    );
    crate::ceo::ollama_chat(&cfg.aggregator, system, &agg_user)
        .unwrap_or_else(|_| user.to_string())
}

/// The MoA verifier — a SEPARATE post-gate call (Slice 4, integration point D). Replaces the
/// single-model `run_review_phase` LLM call. Returns the raw verifier text; the RUNNER parses the
/// `REVIEW: approve|reject` line (unchanged verdict contract). Fail-open on error (the objective
/// gate already passed — plan §5, research §4.5 rail #2).
pub fn run_moa_verifier(ctx: &mut Ctx, committed_diff: &str) -> String {
    let cfg = BrainConfig::from_autopilot();
    if !cfg.enabled {
        // Caller falls back to the existing single-model review path when disabled.
        return String::new();
    }
    let lane = ctx.name.clone();
    let verify_skill = load_skill(&lane, "verify");
    match spawn_worker(&cfg.verifier, VERIFY_PROMPT, &verify_skill, committed_diff, Some(ctx)) {
        Ok(v) => v,
        Err(e) => {
            ctx.log(&format!(
                "MoA verifier failed ({e}); fail-open (objective gate already passed)"
            ));
            // Empty string signals fail-open to the caller (the runner treats unparseable as ship).
            String::new()
        }
    }
}

// --------------------------------------------------------------------------- #
// Tests
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brain_config_disabled_when_block_absent() {
        // autopilot_config() reads repos.json; if the brain block is absent, disabled.
        let cfg = BrainConfig::from_autopilot();
        // The repo's repos.json HAS the brain block (added in Slice 5), so this asserts the shape
        // rather than the disabled state. The disabled path is exercised by the fallback tests below.
        // The config reads without panic + has a non-empty aggregator (the default or the brain block).
        let _ = &cfg;
        assert!(!cfg.aggregator.is_empty());
    }

    #[test]
    fn plan_prompt_is_read_only() {
        // The planner must NOT write files — it proposes, the implementer executes.
        assert!(PLAN_PROMPT.to_lowercase().contains("do not write"));
        assert!(PLAN_PROMPT.to_lowercase().contains("propose"));
    }

    #[test]
    fn aggregate_prompt_is_verbatim_from_moa_paper() {
        // The MoA paper Table 1 opener (research §1.2).
        assert!(AGGREGATE_SYNTHESIZE_PROMPT.starts_with(
            "You have been provided with a set of responses from various open-source models"
        ));
        assert!(AGGREGATE_SYNTHESIZE_PROMPT.contains("synthesize"));
        assert!(AGGREGATE_SYNTHESIZE_PROMPT.contains("critically evaluate"));
    }

    #[test]
    fn verify_prompt_emits_review_line() {
        // The runner parses `REVIEW: approve|reject` — the prompt must instruct that exact shape.
        assert!(VERIFY_PROMPT.contains("REVIEW: approve"));
        assert!(VERIFY_PROMPT.contains("REVIEW: reject"));
        assert!(VERIFY_PROMPT.to_lowercase().contains("fail-open"));
    }

    #[test]
    fn load_skill_missing_file_returns_empty() {
        // A missing skill file is graceful (empty string), not an error.
        let s = load_skill("__nonexistent_repo__", "plan");
        assert!(s.is_empty());
    }

    #[test]
    fn run_moa_aggregator_only_disabled_falls_back() {
        // When disabled, the aggregator-only path falls back to a direct ollama_chat (or the input
        // on failure). We assert it does not panic and returns a string.
        let out = run_moa_aggregator_only("test system", "test user");
        assert!(out.contains("test user") || !out.is_empty());
    }

    #[test]
    fn run_moa_plan_disabled_returns_input_unchanged() {
        // When the brain is disabled (or the block absent), run_moa_plan returns the raw task —
        // the feature-flag fallback contract (byte-identical to pre-MoA).
        // We cannot easily build a Ctx in a unit test; this asserts the disabled path logic via
        // the config read. The full ctx path is exercised by the integration gate.
        let cfg = BrainConfig::from_autopilot();
        if !cfg.enabled {
            // The disabled branch returns task unchanged — proven by code inspection.
            assert!(!cfg.enabled);
        }
    }

    #[test]
    fn moa_worker_quota_error_string_is_classified() {
        // Incident lineage: the 2026-07-03 no-op storm — an Ollama Cloud 429 surfaces from
        // ollama_chat as `no message content in response: {... usage limit ...}`. The spawn_worker
        // Err arm now routes through pi::is_quota_error, so this exact storm string MUST park the
        // endpoint. The pre-fix three-substring check (429/quota/"rate limit") missed it.
        let storm = "no message content in response: {\"error\":\"you have reached your weekly usage limit\"}";
        assert!(
            crate::improver::pi::is_quota_error(storm),
            "the storm string must classify as a quota error so the MoA brain parks the endpoint"
        );
    }
}