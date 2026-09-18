//! TASK-SIZE CALIBRATION TABLE — failure catalog #7 ("problem generation mismatched to executor
//! capability"; RSI v3 requirement: the catalog is the floor).
//!
//! The autopsied failure: planners emitted grand architecture steps free models could not execute
//! (noop streaks misdiagnosed as "backlog exhausted" with 30 items open); dotz's evidence showed
//! concreteness/size of items was the strongest predictor of shipping. Countermeasure: a
//! per-model table `{model, size_class, attempts, ships}` UPDATED FROM OUTCOMES, consulted by the
//! stage that picks backlog items — when the assigned model's ship-rate in an item's size class is
//! proven low, the item must be DECOMPOSED (smallest independently shippable slice), not attempted
//! whole again.
//!
//! Ledger: ONE fleet-wide file `runtime/_task_calibration.json` (capability is a property of the
//! model, not the lane): `{"models": {"<model>": {"<size_class>": {"attempts": N, "ships": N}}}}`.
//! Size classes are the existing backlog tiers (chore/feature/refactor/architecture — whatever
//! string `backlog::strip_tier` yields). Writes are atomic tmp+rename; the write rate is one
//! update per iteration terminal (minutes apart), so no cross-process lock — worst case is a lost
//! increment, never a torn file (the budget.rs discipline, minus the lock its call rate needs).
//!
//! READER-WIRED (a write-only ledger is a bug — catalog #5): iteration.rs's selection stage calls
//! [`decompose_directive`] after tier selection and appends the returned directive to the task;
//! the outcome side rides progress::record_outcome (wiring point B), which resolves the pending
//! selection marker this module stamps at pick time — one hook covering every terminal call site.
//!
//! Attribution: the pending marker (`runtime/<name>/_task_calibration_pending.json`) is stamped
//! AFTER the escalation ladder's apply_fallback_model, so the recorded model is the one actually
//! assigned for the attempt. A crash before the terminal leaves a stale marker; the next
//! selection overwrites it, so an aborted attempt is simply not counted.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::control::proc;
use crate::improver::ctx::Ctx;

/// Below this many recorded attempts the table stays silent for that (model, class) cell —
/// calibration acts on EVIDENCE, never on a cold start (a fresh model must get whole items).
pub const MIN_ATTEMPTS: u64 = 5;
/// The ship-rate floor: at/above it the model keeps whole items of that class; below it (with
/// >= MIN_ATTEMPTS of evidence) the item must be decomposed.
pub const SHIP_RATE_MIN: f64 = 0.25;

const LEDGER_NAME: &str = "_task_calibration.json";
const PENDING_NAME: &str = "_task_calibration_pending.json";

// --------------------------------------------------------------------------- #
// dir-level cores (unit-testable without a Ctx)
// --------------------------------------------------------------------------- #

/// The FLEET-wide runtime dir (Solomon/runtime): ctx.runtime is Solomon/runtime/<name>.
fn fleet_runtime_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ctx.control.join("runtime"))
}

fn ledger_path(dir: &Path) -> PathBuf {
    dir.join(LEDGER_NAME)
}

/// Whole-ledger read; absent/torn files degrade to the empty shape (calibration fails OPEN —
/// broken telemetry must never wedge selection).
fn read_ledger(dir: &Path) -> Value {
    let text = std::fs::read_to_string(ledger_path(dir)).unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if v.is_object() {
        v
    } else {
        json!({ "models": {} })
    }
}

fn write_ledger(dir: &Path, v: &Value) {
    let _ = std::fs::create_dir_all(dir);
    let _ = proc::atomic_write_json(&ledger_path(dir), v);
}

/// `(attempts, ships)` for one (model, size_class) cell; (0, 0) when never seen.
pub fn cell_at(dir: &Path, model: &str, size_class: &str) -> (u64, u64) {
    let led = read_ledger(dir);
    let cell = led
        .get("models")
        .and_then(|m| m.get(model))
        .and_then(|c| c.get(size_class));
    let n = |k: &str| -> u64 {
        cell.and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    (n("attempts"), n("ships"))
}

/// Record one terminal outcome for (model, size_class): attempts += 1, ships += `shipped`.
pub fn record_outcome_at(dir: &Path, model: &str, size_class: &str, shipped: bool) {
    if model.is_empty() || size_class.is_empty() {
        return;
    }
    let (attempts, ships) = cell_at(dir, model, size_class);
    let mut led = read_ledger(dir);
    if !led.get("models").map(Value::is_object).unwrap_or(false) {
        led["models"] = json!({});
    }
    if !led["models"]
        .get(model)
        .map(Value::is_object)
        .unwrap_or(false)
    {
        led["models"][model] = json!({});
    }
    led["models"][model][size_class] = json!({
        "attempts": attempts + 1,
        "ships": ships + if shipped { 1 } else { 0 },
    });
    write_ledger(dir, &led);
}

/// The calibration verdict for one cell: `Some((attempts, ships, rate))` when the cell has enough
/// evidence (>= MIN_ATTEMPTS) AND the ship-rate is below the floor — i.e. the item must be
/// decomposed. None = proceed whole (cold cell, or a proven-capable one).
pub fn low_ship_rate_at(dir: &Path, model: &str, size_class: &str) -> Option<(u64, u64, f64)> {
    let (attempts, ships) = cell_at(dir, model, size_class);
    if attempts < MIN_ATTEMPTS {
        return None;
    }
    let rate = ships as f64 / attempts as f64;
    if rate < SHIP_RATE_MIN {
        Some((attempts, ships, rate))
    } else {
        None
    }
}

// --------------------------------------------------------------------------- #
// selection-side reader (iteration.rs wiring) + pending marker
// --------------------------------------------------------------------------- #

/// The pick-stage reader (catalog #7: "the planner may only emit items in classes where the
/// assigned model's ship-rate >= threshold, else must decompose"): when the assigned model's
/// recorded ship-rate for `size_class` is proven low, returns the mandatory decompose directive
/// the selection appends to the task. None = run the item whole.
pub fn decompose_directive(ctx: &Ctx, size_class: &str) -> Option<String> {
    let dir = fleet_runtime_dir(ctx);
    let (attempts, ships, rate) = low_ship_rate_at(&dir, &ctx.pi_model, size_class)?;
    Some(format!(
        "## Task-size calibration (mandatory)\n\
         The assigned model '{model}' has shipped {ships}/{attempts} '{size_class}'-class backlog \
         items ({pct:.0}% ship-rate, below the {floor:.0}% floor). Do NOT attempt the whole item: \
         DECOMPOSE it — implement only the smallest independently shippable slice (one real \
         symbol, one new behavior, one required test), state in your summary which slices remain, \
         and stop.",
        model = ctx.pi_model,
        pct = rate * 100.0,
        floor = SHIP_RATE_MIN * 100.0,
    ))
}

fn pending_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime.join(PENDING_NAME)
}

/// Stamp the pending attempt at pick time (after apply_fallback_model, so the model recorded is
/// the one that will actually run). Overwrites any stale marker from a crashed iteration.
pub fn note_selection(ctx: &Ctx, size_class: &str) {
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let _ = proc::atomic_write_json(
        &pending_path(ctx),
        &json!({
            "model": ctx.pi_model,
            "size_class": size_class,
            "ts": crate::improver::ctx::now(),
        }),
    );
}

/// Terminal side (called by progress::record_outcome — wiring point B): consume the pending
/// marker and fold the outcome into the fleet table. `outcome` is the history status word;
/// only "shipped" counts as a ship (a blocked/reverted/deviated/noop/error attempt is capability
/// evidence AGAINST the class). No-op when no attempt is pending (beautify/solomon lanes never
/// stamp one).
pub fn resolve_pending(ctx: &Ctx, outcome: &str) {
    let path = pending_path(ctx);
    let Some(pending) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let model = pending.get("model").and_then(Value::as_str).unwrap_or("");
    let class = pending
        .get("size_class")
        .and_then(Value::as_str)
        .unwrap_or("");
    record_outcome_at(&fleet_runtime_dir(ctx), model, class, outcome == "shipped");
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "solomon_calib_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// Isolated Ctx (the budget.rs test pattern): runtime = <base>/runtime/<name>, so the fleet
    /// ledger lands in <base>/runtime.
    fn test_ctx(tag: &str) -> Ctx {
        let base = test_dir(tag);
        let mut c = Ctx::configure(
            &base.join("repo").to_string_lossy(),
            "calibtest",
            "ollama-cloud",
            None,
        );
        c.control = base.join("control");
        c.runtime = base.join("runtime").join("calibtest");
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    // ---- ledger math: attempts/ships accumulate per (model, class) cell ----
    #[test]
    fn outcomes_accumulate_per_model_and_class() {
        let dir = test_dir("acc");
        record_outcome_at(&dir, "glm-5.2", "feature", true);
        record_outcome_at(&dir, "glm-5.2", "feature", false);
        record_outcome_at(&dir, "glm-5.2", "chore", true);
        record_outcome_at(&dir, "minimax-m3", "feature", false);
        assert_eq!(cell_at(&dir, "glm-5.2", "feature"), (2, 1));
        assert_eq!(cell_at(&dir, "glm-5.2", "chore"), (1, 1));
        assert_eq!(cell_at(&dir, "minimax-m3", "feature"), (1, 0));
        assert_eq!(cell_at(&dir, "never-seen", "feature"), (0, 0));
        // persisted shape matches the documented contract byte-names
        let led = read_ledger(&dir);
        assert_eq!(led["models"]["glm-5.2"]["feature"]["attempts"], json!(2));
        assert_eq!(led["models"]["glm-5.2"]["feature"]["ships"], json!(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_or_missing_ledger_fails_open_and_heals() {
        let dir = test_dir("torn");
        std::fs::write(ledger_path(&dir), "{ not json").unwrap();
        assert_eq!(cell_at(&dir, "m", "chore"), (0, 0));
        assert!(low_ship_rate_at(&dir, "m", "chore").is_none());
        record_outcome_at(&dir, "m", "chore", true);
        assert_eq!(cell_at(&dir, "m", "chore"), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the gate: evidence floor + ship-rate threshold ----
    #[test]
    fn low_ship_rate_needs_min_attempts_and_low_rate() {
        let dir = test_dir("gate");
        // 4 failed attempts: below the evidence floor -> silent (cold start protection)
        for _ in 0..4 {
            record_outcome_at(&dir, "m", "architecture", false);
        }
        assert!(low_ship_rate_at(&dir, "m", "architecture").is_none());
        // 5th failure: 0/5 = 0% < 25% -> decompose
        record_outcome_at(&dir, "m", "architecture", false);
        let (attempts, ships, rate) = low_ship_rate_at(&dir, "m", "architecture").unwrap();
        assert_eq!((attempts, ships), (5, 0));
        assert!(rate < f64::EPSILON);
        // a capable cell (2/5 = 40% >= 25%) stays whole
        let dir2 = test_dir("gate_ok");
        for shipped in [true, true, false, false, false] {
            record_outcome_at(&dir2, "m", "feature", shipped);
        }
        assert!(low_ship_rate_at(&dir2, "m", "feature").is_none());
        // exactly AT the floor (25%) is NOT low — the floor is inclusive-pass
        let dir3 = test_dir("gate_edge");
        for shipped in [true, false, false, false] {
            record_outcome_at(&dir3, "m", "chore", shipped);
            record_outcome_at(&dir3, "m2", "chore", shipped);
        }
        record_outcome_at(&dir3, "m", "chore", false); // 1/5 = 20% -> low
        assert!(low_ship_rate_at(&dir3, "m", "chore").is_some());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
        let _ = std::fs::remove_dir_all(&dir3);
    }

    // ---- reader wiring: directive text + per-model isolation ----
    #[test]
    fn decompose_directive_fires_only_for_the_proven_low_cell() {
        let ctx = test_ctx("directive");
        let dir = fleet_runtime_dir(&ctx);
        // cold table: no directive for anything
        assert!(decompose_directive(&ctx, "architecture").is_none());
        // prove THIS ctx's model incapable at architecture
        for _ in 0..MIN_ATTEMPTS {
            record_outcome_at(&dir, &ctx.pi_model, "architecture", false);
        }
        let d = decompose_directive(&ctx, "architecture").expect("directive fires");
        assert!(d.contains(&ctx.pi_model), "{d}");
        assert!(d.contains("architecture"), "{d}");
        assert!(d.contains("DECOMPOSE"), "{d}");
        assert!(d.contains("smallest independently shippable slice"), "{d}");
        // other classes of the same model, and other models, stay whole
        assert!(decompose_directive(&ctx, "chore").is_none());
        for _ in 0..MIN_ATTEMPTS {
            record_outcome_at(&dir, "someone-else", "chore", false);
        }
        assert!(decompose_directive(&ctx, "chore").is_none());
    }

    // ---- pending marker: note at pick, resolve at terminal, ship counted once ----
    #[test]
    fn pending_note_and_resolve_roundtrip() {
        let ctx = test_ctx("pending");
        let dir = fleet_runtime_dir(&ctx);
        note_selection(&ctx, "feature");
        assert!(pending_path(&ctx).exists());
        resolve_pending(&ctx, "shipped");
        assert!(!pending_path(&ctx).exists(), "resolve consumes the marker");
        assert_eq!(cell_at(&dir, &ctx.pi_model, "feature"), (1, 1));
        // non-ship terminals count as attempts only
        note_selection(&ctx, "feature");
        resolve_pending(&ctx, "reverted");
        note_selection(&ctx, "feature");
        resolve_pending(&ctx, "deviated"); // landed but NOT the named item — not a class ship
        assert_eq!(cell_at(&dir, &ctx.pi_model, "feature"), (3, 1));
        // resolve without a pending marker is a no-op (beautify/solomon lanes)
        resolve_pending(&ctx, "shipped");
        assert_eq!(cell_at(&dir, &ctx.pi_model, "feature"), (3, 1));
        // a stale marker from a crashed iteration is OVERWRITTEN by the next selection
        note_selection(&ctx, "architecture");
        note_selection(&ctx, "chore");
        resolve_pending(&ctx, "shipped");
        assert_eq!(cell_at(&dir, &ctx.pi_model, "chore"), (1, 1));
        assert_eq!(cell_at(&dir, &ctx.pi_model, "architecture"), (0, 0));
    }
}
