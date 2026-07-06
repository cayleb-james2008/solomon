//! PROGRESS LEDGER + QUARANTINE — anti retry-theater (failure catalog #5; RSI v3 requirement 3/4).
//!
//! The autopsied failure: a scheduler that re-runs a job whose completion cannot change the state
//! feeding the next scheduling decision (solomon Gen-2 ran one no-op `proof_required` job 303x,
//! starving every AI job queued behind it; asmodeus re-measured the same zero-trade state
//! 288x/day). Countermeasure: a DISK ledger keyed by `hash(job_kind, goal, diagnosis)` — when the
//! same key completes [`QUARANTINE_STRIKES`] times with zero observable state delta, the key is
//! quarantined for [`QUARANTINE_SECS`] and the selection path MUST pick different work. The ledger
//! lives on disk (`runtime/<name>/progress.json`, atomic writes) precisely because the in-memory
//! escalation counters reset on every process restart — the 303x loop survived BECAUSE restarts
//! kept wiping the only memory of it.
//!
//! READER-WIRED: `quarantined()` is consulted by the selection path (iteration.rs wiring point A
//! calls [`filter_quarantined_selection`]) and that wiring is unit-tested below — a write-only
//! ledger is a bug (asmodeus's refutation blocklist was written but never read: 166
//! re-litigations of the same 3 dead families).
//!
//! STATE HASH — "observable state" is the four signals a completed job could plausibly move: the
//! base branch tip (a landed commit), the backlog file bytes (an item ticked/added/deferred), the
//! history.jsonl line count (a recorded terminal outcome), and the last gate counts held in the
//! heartbeat. `record_outcome` must therefore run BEFORE a terminal's own bookkeeping writes
//! (record_history / mark_backlog_done / the escalation ladder's defer) — the iteration.rs call
//! sites honor this ordering so the ledger measures AGENT progress, not the runner's bookkeeping.

use serde_json::{json, Map, Value};
use std::path::PathBuf;

use crate::improver::ctx::{self, Ctx};
use crate::improver::{backlog, escalation};

/// N completed attempts with zero state delta before the key is quarantined (N=3 per catalog #5).
pub const QUARANTINE_STRIKES: i64 = 3;
/// Quarantine duration: 24h (now + 86400 per the catalog countermeasure).
pub const QUARANTINE_SECS: i64 = 86_400;
/// The ledger stores at most this many goal chars — a human-readable sample, not the key source.
const SAMPLE_GOAL_MAX: usize = 160;
/// Goal chars folded into the key (long CEO/campaign items differ early; 200 bounds the digest input).
const GOAL_KEY_MAX: usize = 200;

// --------------------------------------------------------------------------- #
// key + state hash (pure cores, ctx-reading wrappers)
// --------------------------------------------------------------------------- #

/// The quarantine key: `hex(sha1("<job_kind>|<goal[:200]>|<diagnosis>"))`. job_kind is `ctx.phase`
/// (implement/recovery/beautify), goal the selected backlog item text, diagnosis the current
/// escalation category ("" when the lane has no open escalation). Pure — unit-tested for stability.
pub fn progress_key(job_kind: &str, goal: &str, diagnosis: &str) -> String {
    let goal_trunc: String = goal.chars().take(GOAL_KEY_MAX).collect();
    sha1_hex(format!("{job_kind}|{goal_trunc}|{diagnosis}").as_bytes())
}

/// The current escalation category from `runtime/<name>/escalation.json` (the supervisor's
/// diagnosis for this lane), "" when absent/unparseable. Folding it into the key means a NEW
/// diagnosis legitimately re-opens work an old diagnosis quarantined.
pub fn current_diagnosis(ctx: &Ctx) -> String {
    std::fs::read_to_string(ctx.runtime.join("escalation.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("category").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// [`progress_key`] for THIS lane's phase + current diagnosis.
pub fn selection_key(ctx: &Ctx, goal: &str) -> String {
    progress_key(&ctx.phase, goal, &current_diagnosis(ctx))
}

/// sha1 over the four observable-state components. The base tip is resolved via
/// `rev-parse <base_branch>` (not HEAD): at selection time the loop sits on a fresh rsi/* branch
/// and at terminal time on whatever drop_branch/ship left checked out — the BASE ref is the one
/// signal that is invariant to the loop's own checkout churn.
pub fn state_hash(ctx: &Ctx) -> String {
    let base_sha = ctx
        .git(&["rev-parse", &ctx.base_branch], 120)
        .stdout
        .trim()
        .to_string();
    let backlog_sha = match std::fs::read(&ctx.backlog) {
        Ok(bytes) => sha1_hex(&bytes),
        Err(_) => "-".to_string(), // missing backlog => "-" (per contract)
    };
    let history_lines = std::fs::read_to_string(ctx.runtime.join("history.jsonl"))
        .map(|c| c.lines().count())
        .unwrap_or(0);
    let gate_counts = match ctx.hb.get("tests") {
        Some(v) if !v.is_null() => serde_json::to_string(v).unwrap_or_else(|_| "-".to_string()),
        _ => "-".to_string(),
    };
    compose_state_hash(&base_sha, &backlog_sha, history_lines, &gate_counts)
}

/// The pure combiner behind [`state_hash`] — split out so the per-component sensitivity is
/// unit-testable without a git repo.
fn compose_state_hash(
    base_sha: &str,
    backlog_sha: &str,
    history_lines: usize,
    gate_counts: &str,
) -> String {
    sha1_hex(format!("{base_sha}|{backlog_sha}|{history_lines}|{gate_counts}").as_bytes())
}

// --------------------------------------------------------------------------- #
// the ledger (runtime/<name>/progress.json, atomic writes)
// --------------------------------------------------------------------------- #

fn ledger_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime.join("progress.json")
}

/// The whole ledger object; a missing/torn file yields the empty shape (the ledger fails OPEN —
/// broken telemetry must never wedge selection).
fn read_ledger_value(ctx: &Ctx) -> Value {
    std::fs::read_to_string(ledger_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "keys": {} }))
}

fn write_ledger_value(ctx: &Ctx, led: &Value) {
    let text = serde_json::to_string_pretty(led).unwrap_or_else(|_| "{}".to_string());
    ctx.runtime_atomic_write(&ledger_path(ctx), &text);
}

/// The mutable `"keys"` map, created when absent (a hand-edited ledger without it stays usable).
fn ensure_keys(led: &mut Value) -> &mut Map<String, Value> {
    let needs_seed = !led.get("keys").map(Value::is_object).unwrap_or(false);
    if needs_seed {
        if let Value::Object(o) = led {
            o.insert("keys".to_string(), json!({}));
        }
    }
    led.get_mut("keys")
        .and_then(Value::as_object_mut)
        .expect("keys map ensured above")
}

fn fresh_entry(now_iso: &str, sample_goal: &str) -> Value {
    json!({
        "count_no_delta": 0,
        "last_state_hash": "",
        "quarantined_until": 0,
        "first_seen": now_iso,
        "last_seen": now_iso,
        "sample_goal": sample_goal,
    })
}

// --------------------------------------------------------------------------- #
// API: quarantined / note_selected / record_outcome
// --------------------------------------------------------------------------- #

/// True iff `key` is currently quarantined (`quarantined_until` is in the future). An empty key
/// (beautify/solomon lanes compute none) and a missing entry are never quarantined.
pub fn quarantined(ctx: &Ctx, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    read_ledger_value(ctx)
        .get("keys")
        .and_then(|k| k.get(key))
        .and_then(|e| e.get("quarantined_until"))
        .and_then(Value::as_i64)
        .map(|t| t > unix_now())
        .unwrap_or(false)
}

/// Seed/refresh the ledger entry at selection time: `sample_goal` (truncated, for the QUARANTINE
/// log and the operator reading the ledger) + `first_seen`/`last_seen`. Never touches the counter.
pub fn note_selected(ctx: &Ctx, key: &str, goal: &str) {
    if key.is_empty() {
        return;
    }
    let mut led = read_ledger_value(ctx);
    let now_iso = ctx::now();
    let sample: String = goal.chars().take(SAMPLE_GOAL_MAX).collect();
    {
        let keys = ensure_keys(&mut led);
        match keys.get_mut(key).and_then(Value::as_object_mut) {
            Some(entry) => {
                entry.insert("last_seen".to_string(), json!(now_iso));
                entry.insert("sample_goal".to_string(), json!(sample));
            }
            None => {
                keys.insert(key.to_string(), fresh_entry(&now_iso, &sample));
            }
        }
    }
    write_ledger_value(ctx, &led);
}

/// Record a terminal outcome for `key`. `outcome` is one of the existing history status words
/// {shipped, reverted, noop, deviated, blocked, error}. A non-"shipped" outcome whose post-state
/// hash equals `pre_hash` (captured at selection) is a zero-delta completion: the strike counter
/// increments and at [`QUARANTINE_STRIKES`] the key is quarantined for [`QUARANTINE_SECS`].
/// Anything else — a ship, or ANY observable state delta — resets the counter and clears the
/// quarantine. No-op on an empty key.
pub fn record_outcome(ctx: &mut Ctx, key: &str, pre_hash: &str, outcome: &str) {
    if key.is_empty() {
        return;
    }
    // Post-state FIRST, before this function's own ledger write could ever grow into a component.
    let post_hash = state_hash(ctx);
    let no_delta = outcome != "shipped" && post_hash == pre_hash;
    let now_iso = ctx::now();
    let mut led = read_ledger_value(ctx);
    let mut quarantine_log: Option<String> = None;
    {
        let keys = ensure_keys(&mut led);
        if !keys.get(key).map(Value::is_object).unwrap_or(false) {
            // defensive: a terminal firing for a never-seeded key (hand-cleared ledger mid-run)
            keys.insert(key.to_string(), fresh_entry(&now_iso, ""));
        }
        let entry = keys
            .get_mut(key)
            .and_then(Value::as_object_mut)
            .expect("entry ensured above");
        let mut count = entry
            .get("count_no_delta")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let mut until = entry
            .get("quarantined_until")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if no_delta {
            count += 1;
            if count >= QUARANTINE_STRIKES {
                // >= (not ==): after an expiry, a further zero-delta completion re-arms the 24h.
                until = unix_now() + QUARANTINE_SECS;
                let sample = entry
                    .get("sample_goal")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                quarantine_log = Some(format!(
                    "QUARANTINE: key {key} ({sample}) completed {count}x with zero observable \
state delta — scheduler must select different work"
                ));
            }
        } else {
            count = 0;
            until = 0;
        }
        entry.insert("count_no_delta".to_string(), json!(count));
        entry.insert("last_state_hash".to_string(), json!(post_hash));
        entry.insert("quarantined_until".to_string(), json!(until));
        entry.insert("last_seen".to_string(), json!(now_iso));
    }
    write_ledger_value(ctx, &led);
    // TASK-SIZE CALIBRATION (catalog #7, terminal side): every iteration terminal already funnels
    // through here, so this ONE hook folds the outcome into the fleet's per-model
    // {size_class, attempts, ships} table (calibration.rs stamped the pending attempt at pick
    // time; only "shipped" counts as a ship). No-op when nothing is pending.
    crate::improver::calibration::resolve_pending(ctx, outcome);
    if let Some(line) = quarantine_log {
        ctx.log(&line);
    }
}

// --------------------------------------------------------------------------- #
// selection-path wiring (iteration.rs wiring point A calls this)
// --------------------------------------------------------------------------- #

/// What wiring point A hands back to the iteration when work may proceed.
pub struct Selection {
    pub goal: String,
    pub tier: String,
    pub key: String,
    pub pre_hash: String,
}

/// Quarantine-filter the first-selected backlog item. Not quarantined => proceed with it. If it IS
/// quarantined: defer it (so `top_backlog_item` yields the next item) and re-select ONCE; if the
/// re-selected key is ALSO quarantined, write the `all_quarantined` idle heartbeat + log and
/// return None — the caller returns without any pi spend. The returned `pre_hash` is computed
/// AFTER any defer (the defer rewrites the backlog, which is a state-hash component).
pub fn filter_quarantined_selection(ctx: &mut Ctx, goal: String, tier: String) -> Option<Selection> {
    let key = selection_key(ctx, &goal);
    if !quarantined(ctx, &key) {
        let pre_hash = state_hash(ctx);
        note_selected(ctx, &key, &goal);
        return Some(Selection { goal, tier, key, pre_hash });
    }
    ctx.log(&format!(
        "progress: top backlog item '{}' is QUARANTINED (key {key}) — deferring and re-selecting once",
        head_chars(&goal, 60)
    ));
    escalation::defer_backlog_item(ctx, &goal);
    let (goal2, tier2) = backlog::top_backlog_item(ctx)
        .unwrap_or_else(|| ("model-chosen improvement".to_string(), "chore".to_string()));
    let key2 = selection_key(ctx, &goal2);
    if quarantined(ctx, &key2) {
        all_quarantined_bail(ctx);
        return None;
    }
    let pre_hash = state_hash(ctx);
    note_selected(ctx, &key2, &goal2);
    Some(Selection { goal: goal2, tier: tier2, key: key2, pre_hash })
}

/// The no-work terminal of wiring point A: every selectable head is quarantined — idle out loudly
/// (zero pi spend). HONEST about the degraded mode: the lane idles until a quarantine expires
/// (24h) or new backlog items arrive (ideate_phase's normal refill valve, when the pipeline has it
/// enabled). It does NOT claim to force ideation — no such gating marker exists, and inventing
/// one here would be a write-only lever (the previous text/marker-delete were exactly that:
/// skeptic finding 8, 2026-07-06).
fn all_quarantined_bail(ctx: &mut Ctx) {
    ctx.heartbeat(json!({
        "status": "idle",
        "phase": Value::Null,
        "reason": "all_quarantined",
        "last_summary": "top backlog keys are quarantined (no state delta in 3 attempts each) — \
idling until a quarantine expires (24h) or new backlog items arrive",
    }));
    ctx.log(
        "SKIP iteration: all_quarantined — top backlog keys are quarantined (no state delta in \
3 attempts each); idling until a quarantine expires (24h) or new backlog items arrive (no pi spend)",
    );
}

// --------------------------------------------------------------------------- #
// small helpers
// --------------------------------------------------------------------------- #

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// First `n` chars by code points (a mid-UTF-8 byte slice would panic).
fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// SHA-1 (FIPS 180-1), lowercase hex. Implemented locally because the workspace deliberately has
/// no hashing dependency; SHA-1 here is a stable fingerprint for ledger keys/state — not a
/// security boundary. Pinned to the standard test vectors below.
fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let j = 4 * i;
            *word = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = String::with_capacity(40);
    for x in h {
        out.push_str(&format!("{x:08x}"));
    }
    out
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test unique suffix so parallel tests never share a tmp dir (freshness.rs pattern).
    fn uniq() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("{:x}_{:x}", nanos, N.fetch_add(1, Ordering::Relaxed))
    }

    /// An isolated Ctx over injected tmp paths: its own runtime dir, backlog/lessons dir, and an
    /// EXISTING (non-git) repo dir — `ctx.git` then deterministically yields an empty base sha,
    /// keeping every state-hash component test-controlled.
    fn test_ctx() -> Ctx {
        let base = std::env::temp_dir().join(format!("solomon_progress_test_{}", uniq()));
        let control = base.join("control");
        let repo = base.join("repo");
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::create_dir_all(&repo);
        let mut c = Ctx::configure(&repo.to_string_lossy(), "progtest", "ollama-cloud", None);
        c.control = control;
        c.runtime = base.join("runtime").join("progtest");
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        c.backlog = base.join("improver").join("progtest").join("backlog.md");
        c.lessons = base.join("improver").join("progtest").join("LESSONS.md");
        let _ = std::fs::create_dir_all(c.backlog.parent().unwrap());
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    /// Force a key into quarantine directly (entry seeded through the module's own writer).
    fn seed_quarantine(c: &Ctx, key: &str, until: i64) {
        note_selected(c, key, "seeded");
        let mut led = read_ledger_value(c);
        led["keys"][key]["quarantined_until"] = json!(until);
        led["keys"][key]["count_no_delta"] = json!(QUARANTINE_STRIKES);
        write_ledger_value(c, &led);
    }

    fn entry_i64(c: &Ctx, key: &str, field: &str) -> i64 {
        read_ledger_value(c)["keys"][key][field].as_i64().unwrap_or(-1)
    }

    fn hb_str<'a>(c: &'a Ctx, key: &str) -> &'a str {
        c.hb.get(key).and_then(Value::as_str).unwrap_or("")
    }

    // ---- sha1 (the local implementation is pinned to the standard vectors) ----

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
        // multi-block input (>64 bytes) exercises the chunk loop
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    // ---- key stability + sensitivity + truncation ----

    #[test]
    fn key_stability_and_component_sensitivity() {
        let k = progress_key("implement", "fix the widget", "");
        assert_eq!(k, progress_key("implement", "fix the widget", ""), "stable");
        assert_eq!(k.len(), 40);
        assert_ne!(k, progress_key("recovery", "fix the widget", ""), "job_kind");
        assert_ne!(k, progress_key("implement", "fix the gadget", ""), "goal");
        assert_ne!(k, progress_key("implement", "fix the widget", "gate_red"), "diagnosis");
    }

    #[test]
    fn key_truncates_goal_at_200_chars() {
        let base: String = "g".repeat(200);
        let a = format!("{base}AAAA");
        let b = format!("{base}BBBB");
        assert_eq!(
            progress_key("implement", &a, ""),
            progress_key("implement", &b, ""),
            "chars beyond 200 must not change the key"
        );
        let short_a = format!("{}A", "g".repeat(199));
        let short_b = format!("{}B", "g".repeat(199));
        assert_ne!(
            progress_key("implement", &short_a, ""),
            progress_key("implement", &short_b, ""),
            "char 200 is still inside the key"
        );
    }

    // ---- state hash: sensitivity to each component ----

    #[test]
    fn compose_state_hash_sensitive_to_each_component() {
        let h = compose_state_hash("sha", "bsha", 7, "{\"passed\":1}");
        assert_eq!(h, compose_state_hash("sha", "bsha", 7, "{\"passed\":1}"));
        assert_ne!(h, compose_state_hash("SHA2", "bsha", 7, "{\"passed\":1}"), "base sha");
        assert_ne!(h, compose_state_hash("sha", "other", 7, "{\"passed\":1}"), "backlog sha");
        assert_ne!(h, compose_state_hash("sha", "bsha", 8, "{\"passed\":1}"), "history lines");
        assert_ne!(h, compose_state_hash("sha", "bsha", 7, "{\"passed\":2}"), "gate counts");
    }

    #[test]
    fn state_hash_reads_the_live_ctx_components() {
        let mut c = test_ctx();
        let h0 = state_hash(&c);
        assert_eq!(h0, state_hash(&c), "deterministic over unchanged state");
        // backlog bytes
        std::fs::write(&c.backlog, "# backlog\n").unwrap();
        let h1 = state_hash(&c);
        assert_ne!(h1, h0, "backlog write must move the hash");
        // history line count
        std::fs::write(c.runtime.join("history.jsonl"), "{\"status\":\"noop\"}\n").unwrap();
        let h2 = state_hash(&c);
        assert_ne!(h2, h1, "history append must move the hash");
        // last gate counts in the heartbeat
        if let Value::Object(hb) = &mut c.hb {
            hb.insert("tests".to_string(), json!({"passed": 3, "failed": 0}));
        }
        let h3 = state_hash(&c);
        assert_ne!(h3, h2, "gate counts must move the hash");
    }

    // ---- 3-strikes quarantine / shipped reset / delta reset / expiry ----

    #[test]
    fn three_zero_delta_completions_quarantine_the_key() {
        let mut c = test_ctx();
        let key = selection_key(&c, "fix the flaky widget test");
        note_selected(&c, &key, "fix the flaky widget test");
        let pre = state_hash(&c);
        record_outcome(&mut c, &key, &pre, "noop");
        record_outcome(&mut c, &key, &pre, "reverted");
        assert!(!quarantined(&c, &key), "2 strikes is below the limit");
        assert_eq!(entry_i64(&c, &key, "count_no_delta"), 2);
        record_outcome(&mut c, &key, &pre, "noop");
        assert!(quarantined(&c, &key), "3rd zero-delta completion quarantines");
        assert!(entry_i64(&c, &key, "quarantined_until") > unix_now());
        let log = std::fs::read_to_string(&c.log_path).unwrap_or_default();
        assert!(
            log.contains(&format!("QUARANTINE: key {key} (fix the flaky widget test)")),
            "log: {log}"
        );
        assert!(log.contains("completed 3x with zero observable state delta"), "log: {log}");
    }

    #[test]
    fn shipped_resets_the_counter_and_clears_quarantine() {
        let mut c = test_ctx();
        let key = selection_key(&c, "item that finally ships");
        note_selected(&c, &key, "item that finally ships");
        let pre = state_hash(&c);
        record_outcome(&mut c, &key, &pre, "noop");
        record_outcome(&mut c, &key, &pre, "noop");
        assert_eq!(entry_i64(&c, &key, "count_no_delta"), 2);
        // a ship resets even when the state hash did not move
        record_outcome(&mut c, &key, &pre, "shipped");
        assert_eq!(entry_i64(&c, &key, "count_no_delta"), 0);
        assert_eq!(entry_i64(&c, &key, "quarantined_until"), 0);
        assert!(!quarantined(&c, &key));
    }

    #[test]
    fn any_state_delta_resets_and_clears_an_active_quarantine() {
        let mut c = test_ctx();
        let key = selection_key(&c, "quarantined then the world moved");
        seed_quarantine(&c, &key, unix_now() + 3600);
        assert!(quarantined(&c, &key));
        // pre_hash deliberately different from the live post hash => observable delta
        record_outcome(&mut c, &key, "a-stale-pre-hash-that-cannot-match", "reverted");
        assert_eq!(entry_i64(&c, &key, "count_no_delta"), 0);
        assert_eq!(entry_i64(&c, &key, "quarantined_until"), 0);
        assert!(!quarantined(&c, &key), "a real delta clears the quarantine");
    }

    #[test]
    fn quarantine_expires_by_wall_clock() {
        let c = test_ctx();
        let key = progress_key("implement", "expired item", "");
        seed_quarantine(&c, &key, unix_now() - 10);
        assert!(!quarantined(&c, &key), "past quarantined_until => selectable again");
        seed_quarantine(&c, &key, unix_now() + 1000);
        assert!(quarantined(&c, &key), "future quarantined_until => blocked");
    }

    #[test]
    fn empty_key_is_inert() {
        // beautify/solomon lanes compute no key — every API must no-op, creating no ledger.
        let mut c = test_ctx();
        assert!(!quarantined(&c, ""));
        note_selected(&c, "", "goal");
        record_outcome(&mut c, "", "pre", "noop");
        assert!(!ledger_path(&c).exists(), "no ledger file written for an empty key");
    }

    #[test]
    fn note_selected_seeds_and_truncates_sample_goal() {
        let c = test_ctx();
        let long_goal: String = "x".repeat(400);
        let key = progress_key("implement", &long_goal, "");
        note_selected(&c, &key, &long_goal);
        let led = read_ledger_value(&c);
        let entry = &led["keys"][&key];
        assert_eq!(
            entry["sample_goal"].as_str().unwrap().chars().count(),
            SAMPLE_GOAL_MAX,
            "sample_goal is capped at 160 chars"
        );
        assert_eq!(entry["count_no_delta"], json!(0));
        let first_seen = entry["first_seen"].as_str().unwrap().to_string();
        assert!(!first_seen.is_empty());
        // a re-selection refreshes last_seen but preserves first_seen
        note_selected(&c, &key, &long_goal);
        let led2 = read_ledger_value(&c);
        assert_eq!(led2["keys"][&key]["first_seen"].as_str().unwrap(), first_seen);
    }

    #[test]
    fn current_diagnosis_reads_the_escalation_category() {
        let c = test_ctx();
        assert_eq!(current_diagnosis(&c), "", "no escalation.json => empty diagnosis");
        std::fs::write(
            c.runtime.join("escalation.json"),
            r#"{"category": "gate_red_persistent", "evidence": "x"}"#,
        )
        .unwrap();
        assert_eq!(current_diagnosis(&c), "gate_red_persistent");
        assert_ne!(
            selection_key(&c, "same goal"),
            progress_key(&c.phase, "same goal", ""),
            "a live diagnosis changes the selection key"
        );
    }

    // ---- READER-WIRED: a quarantined key causes selection to defer ----

    const TWO_ITEM_BACKLOG: &str = "# backlog\n\n\
- [ ] [feature] alpha improve the frobnicator pipeline end to end\n\
- [ ] [chore] beta tidy the developer docs\n";

    #[test]
    fn quarantined_key_defers_and_reselects_the_next_item() {
        let mut c = test_ctx();
        std::fs::write(&c.backlog, TWO_ITEM_BACKLOG).unwrap();
        let (g1, t1) = backlog::top_backlog_item(&c).unwrap();
        assert!(g1.starts_with("alpha"), "got: {g1}");
        seed_quarantine(&c, &selection_key(&c, &g1), unix_now() + 3600);

        let sel = filter_quarantined_selection(&mut c, g1.clone(), t1)
            .expect("second item is selectable");
        assert!(sel.goal.starts_with("beta"), "re-selected the next item, got: {}", sel.goal);
        assert_eq!(sel.tier, "chore");
        assert_eq!(sel.key, selection_key(&c, &sel.goal));
        assert_eq!(sel.pre_hash.len(), 40, "pre-hash captured for the terminals");
        // the quarantined item was DEFERRED (moved to the bottom with the deferred note)
        let text = std::fs::read_to_string(&c.backlog).unwrap();
        assert!(text.contains("(deferred"), "backlog: {text}");
        let last_item_line = text.lines().rev().find(|l| l.contains("- [ ]")).unwrap();
        assert!(last_item_line.contains("alpha"), "alpha sits at the bottom: {last_item_line}");
        // and NOT the all_quarantined bail
        assert_ne!(hb_str(&c, "reason"), "all_quarantined");
    }

    #[test]
    fn unquarantined_selection_passes_through_and_seeds_the_ledger() {
        let mut c = test_ctx();
        std::fs::write(&c.backlog, TWO_ITEM_BACKLOG).unwrap();
        let (g1, t1) = backlog::top_backlog_item(&c).unwrap();
        let sel = filter_quarantined_selection(&mut c, g1.clone(), t1).expect("proceeds");
        assert_eq!(sel.goal, g1, "top item kept when not quarantined");
        let led = read_ledger_value(&c);
        assert!(
            led["keys"][&sel.key]["sample_goal"].as_str().unwrap().starts_with("alpha"),
            "selection seeds the entry (sample_goal) for the terminal record_outcome"
        );
    }

    #[test]
    fn all_quarantined_idles_out_with_an_honest_degraded_mode_summary() {
        let mut c = test_ctx();
        std::fs::write(&c.backlog, TWO_ITEM_BACKLOG).unwrap();
        let (g1, t1) = backlog::top_backlog_item(&c).unwrap();
        seed_quarantine(&c, &selection_key(&c, &g1), unix_now() + 3600);
        seed_quarantine(
            &c,
            &selection_key(&c, "beta tidy the developer docs"),
            unix_now() + 3600,
        );

        assert!(
            filter_quarantined_selection(&mut c, g1, t1).is_none(),
            "both heads quarantined => no selection, no pi spend"
        );
        assert_eq!(hb_str(&c, "status"), "idle");
        assert!(c.hb.get("phase").map(Value::is_null).unwrap_or(false), "phase: null");
        assert_eq!(hb_str(&c, "reason"), "all_quarantined");
        // the summary must describe the REAL degraded mode (24h expiry / new items), never a
        // "forcing ideate" no-op lever that does not exist (skeptic finding 8, 2026-07-06)
        let summary = hb_str(&c, "last_summary");
        assert!(summary.contains("idling until a quarantine expires"), "{summary}");
        assert!(!summary.contains("forcing ideate"), "{summary}");
    }

    // ---- CALIBRATION WIRING (catalog #7): record_outcome resolves the pending attempt ----
    #[test]
    fn record_outcome_folds_the_pending_calibration_attempt_into_the_fleet_table() {
        let mut c = test_ctx();
        std::fs::write(&c.backlog, TWO_ITEM_BACKLOG).unwrap();
        let key = selection_key(&c, "alpha improve the frobnicator pipeline end to end");
        note_selected(&c, &key, "alpha improve the frobnicator pipeline end to end");
        crate::improver::calibration::note_selection(&c, "feature");
        let fleet_dir = c.runtime.parent().unwrap().to_path_buf();

        record_outcome(&mut c, &key, "different-pre-hash", "shipped");
        assert_eq!(
            crate::improver::calibration::cell_at(&fleet_dir, &c.pi_model, "feature"),
            (1, 1),
            "a shipped terminal records attempts+1, ships+1 via the record_outcome hook"
        );
        // a non-ship terminal with a fresh pending marker counts as an attempt only
        crate::improver::calibration::note_selection(&c, "feature");
        record_outcome(&mut c, &key, "different-pre-hash", "reverted");
        assert_eq!(
            crate::improver::calibration::cell_at(&fleet_dir, &c.pi_model, "feature"),
            (2, 1)
        );
    }
}
