//! Provider budget ledger + capability canary (RSI v3 requirement 4; failure catalog #2
//! "provider quota monoculture → capability collapse → fleet death").
//!
//! ONE fleet-wide ledger at runtime/_provider_budget.json tracks every "<provider>:<model>"
//! endpoint the fleet has ever touched: a weekly call cap (reserve headroom: never PLAN a call past
//! the cap), per-endpoint exponential 429 parking, and the last time the endpoint passed a
//! capability canary. Each rule exists because its absence killed a fleet:
//!   - one 429 killed the whole Gen-1 fleet simultaneously → a quota error parks only the endpoint
//!     that 429'd, NEVER the fleet;
//!   - Solomon Gen-2's 24h blanket cooldown put the fleet to sleep a day on one quota error → the
//!     park schedule is 900*2^(n-1)s capped at 21600s (15min exponential, 6h cap — explicitly NOT
//!     an 86400s blanket);
//!   - asmodeus's fallback failed 78/78 calls with 0 tokens (a dead endpoint nobody canaried) → a
//!     fallback is adopted only after a DIFF-VERIFIED capability canary (the agent must really
//!     write CANARY.txt with a nonce; narration counts for nothing) passed within the last 24h.
//!
//! Cross-process safety: the ledger is guarded by an O_EXCL lockfile runtime/_provider_budget.lock
//! (max 5s wait; locks older than 30s are broken — a crashed holder must not wedge the fleet), and
//! every write is atomic tmp+rename so readers can never observe a torn file even when the lock was
//! lost. On lock timeout operations proceed WITHOUT the lock (fail open): the worst case is a lost
//! counter increment, never a torn ledger — a wedged lock must not stop token accounting fleet-wide.
//!
//! Wiring: pi.rs::run_pi is the ONLY caller — it resolves the endpoint via [`effective_endpoint`],
//! refuses parked endpoints with the synthesized [`parked_stderr`] (its literal "429" makes the
//! EXISTING is_quota_error/is_quota_error_output classification treat the refusal as quota_error
//! with zero new wiring), meters real spawns via [`record_call`], and classifies outcomes into
//! [`record_quota`] / [`record_success`].

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::control::proc;
use crate::improver::ctx::Ctx;
use crate::improver::gitops;
use crate::improver::pi;

/// Weekly cap window length (seconds). The shared Ollama-cloud account caps usage per WEEK — the
/// 2026-07-03 fleet outage was its weekly cap — so the ledger's window matches that period.
const WEEK_S: u64 = 604_800;
/// Calls per endpoint per window when an endpoint is first seen. Conservative: the reserve-headroom
/// rule refuses to PLAN a call past this, so an unknown endpoint can never be driven to a hard 429
/// storm by the fleet itself.
const DEFAULT_WINDOW_CAP: u64 = 500;
/// First 429 park (seconds): 15 minutes.
const PARK_BASE_S: u64 = 900;
/// Park ceiling (seconds): 6 hours — explicitly NOT Gen-2's 86400s blanket.
const PARK_CAP_S: u64 = 21_600;
/// A fallback's canary pass is trusted for this long (24h, per the catalog #2 countermeasure).
const CANARY_FRESH_S: u64 = 86_400;
/// Fixed canary wall clock: one tiny file-write task must not need more than 5 minutes.
const CANARY_TIMEOUT_S: i64 = 300;
/// Max wait for the ledger lock before proceeding lockless (fail open).
const LOCK_WAIT_MS: u64 = 5_000;
/// A lock older than this belongs to a dead holder and is broken.
const LOCK_STALE_S: u64 = 30;

const LEDGER_NAME: &str = "_provider_budget.json";
const LOCK_NAME: &str = "_provider_budget.lock";

// --------------------------------------------------------------------------- #
// Decision + endpoint state
// --------------------------------------------------------------------------- #

/// Preflight verdict for one endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Proceed,
    Parked { until: u64, why: String },
}

impl Decision {
    pub fn is_parked(&self) -> bool {
        matches!(self, Decision::Parked { .. })
    }
}

/// One ledger row. Field names match the persisted JSON byte-for-byte (dashboard-readable).
#[derive(Debug, Clone, PartialEq)]
struct Endpoint {
    window_cap_calls: u64,
    window_started: u64,
    spent_calls: u64,
    park_until: u64,
    consecutive_429: u64,
    last_canary_pass: u64,
}

fn endpoint_key(provider: &str, model: &str) -> String {
    format!("{provider}:{model}")
}

fn u64_field(o: &Map<String, Value>, k: &str) -> Option<u64> {
    o.get(k).and_then(Value::as_u64)
}

/// Seed-or-parse: a missing/torn row seeds the documented defaults (cap 500/week, window starts
/// now) so an endpoint is budgeted from FIRST sight, never after its first incident.
fn endpoint_from(v: Option<&Value>, now: u64) -> Endpoint {
    match v.and_then(Value::as_object) {
        None => Endpoint {
            window_cap_calls: DEFAULT_WINDOW_CAP,
            window_started: now,
            spent_calls: 0,
            park_until: 0,
            consecutive_429: 0,
            last_canary_pass: 0,
        },
        Some(o) => Endpoint {
            window_cap_calls: u64_field(o, "window_cap_calls").unwrap_or(DEFAULT_WINDOW_CAP),
            window_started: u64_field(o, "window_started").unwrap_or(now),
            spent_calls: u64_field(o, "spent_calls").unwrap_or(0),
            park_until: u64_field(o, "park_until").unwrap_or(0),
            consecutive_429: u64_field(o, "consecutive_429").unwrap_or(0),
            last_canary_pass: u64_field(o, "last_canary_pass").unwrap_or(0),
        },
    }
}

fn endpoint_to_value(ep: &Endpoint) -> Value {
    json!({
        "window_cap_calls": ep.window_cap_calls,
        "window_started": ep.window_started,
        "spent_calls": ep.spent_calls,
        "park_until": ep.park_until,
        "consecutive_429": ep.consecutive_429,
        "last_canary_pass": ep.last_canary_pass,
    })
}

/// Window rollover: when now-window_started > 604800 the week is over — reset spent (and restart
/// the window at now). Park state is NOT touched: a 429 backoff outlives a coincidental rollover.
fn rollover(ep: &mut Endpoint, now: u64) {
    if now.saturating_sub(ep.window_started) > WEEK_S {
        ep.window_started = now;
        ep.spent_calls = 0;
    }
}

/// The park schedule: min(900 * 2^(n-1), 21600) for the n-th consecutive 429.
/// 900/1800/3600/7200/14400 then capped at 21600 forever.
fn park_backoff_s(consecutive_429: u64) -> u64 {
    if consecutive_429 == 0 {
        return PARK_BASE_S; // defensive: a 0-count park is still the base park, never 0s
    }
    // shift clamp keeps 900<<n from overflowing long before .min() applies
    PARK_BASE_S
        .saturating_mul(1u64 << (consecutive_429 - 1).min(20))
        .min(PARK_CAP_S)
}

/// The preflight rule: parked while a 429 backoff is live, OR when the window's call budget is
/// spent (reserve headroom: never PLAN a call past the cap — the cap-th call is the last allowed).
fn decide(ep: &Endpoint, now: u64) -> Decision {
    if now < ep.park_until {
        return Decision::Parked {
            until: ep.park_until,
            why: format!(
                "endpoint parked after {} consecutive 429(s)",
                ep.consecutive_429
            ),
        };
    }
    if ep.spent_calls >= ep.window_cap_calls {
        return Decision::Parked {
            until: ep.window_started + WEEK_S,
            why: format!(
                "weekly call cap exhausted ({}/{} calls this window)",
                ep.spent_calls, ep.window_cap_calls
            ),
        };
    }
    Decision::Proceed
}

// --------------------------------------------------------------------------- #
// ledger IO: O_EXCL lock + atomic tmp+rename writes
// --------------------------------------------------------------------------- #

fn ledger_path(dir: &Path) -> PathBuf {
    dir.join(LEDGER_NAME)
}

/// Removes the lockfile on drop, releasing the O_EXCL claim.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// O_EXCL (`create_new`) lock acquisition: wait up to `max_wait`, breaking any lock whose mtime is
/// older than `stale_after` (its holder crashed mid-write; leaked O_EXCL locks otherwise wedge the
/// whole fleet's token accounting forever). Returns None on timeout — callers proceed lockless
/// because every ledger write is atomic tmp+rename (worst case a lost increment, never a torn file).
fn acquire_lock(dir: &Path, max_wait: Duration, stale_after: Duration) -> Option<LockGuard> {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(LOCK_NAME);
    let deadline = Instant::now() + max_wait;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // O_EXCL: exactly one winner across processes
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                // holder provenance (debug aid only; staleness is judged by mtime, not content)
                let _ = write!(f, "{} {}", std::process::id(), unix_now());
                return Some(LockGuard { path });
            }
            Err(_) => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map(|e| e >= stale_after)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue; // re-race the O_EXCL create — another waiter may win, that's fine
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Whole-ledger read; absent/torn files degrade to the empty ledger (the fleet must keep moving —
/// endpoints reseed on next sight; atomic writes make "torn" nearly impossible anyway).
fn read_ledger(dir: &Path) -> Value {
    let text = std::fs::read_to_string(ledger_path(dir)).unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if v.is_object() {
        v
    } else {
        json!({ "endpoints": {} })
    }
}

/// Atomic tmp+rename write (rename replaces on Windows too), same discipline as the heartbeat.
fn write_ledger(dir: &Path, v: &Value) {
    let _ = std::fs::create_dir_all(dir);
    let path = ledger_path(dir);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string());
    if std::fs::write(&tmp, text.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Locked read-modify-write around ONE endpooint row: seed on first sight, roll the window over,
/// apply `f`, persist. All public mutations and preflights funnel through here so every path gets
/// identical seeding/rollover semantics.
fn with_endpoint<T>(
    dir: &Path,
    provider: &str,
    model: &str,
    now: u64,
    f: impl FnOnce(&mut Endpoint) -> T,
) -> T {
    let _lock = acquire_lock(
        dir,
        Duration::from_millis(LOCK_WAIT_MS),
        Duration::from_secs(LOCK_STALE_S),
    );
    let mut root = read_ledger(dir);
    let key = endpoint_key(provider, model);
    let mut ep = endpoint_from(
        root.get("endpoints").and_then(|e| e.get(&key)),
        now,
    );
    rollover(&mut ep, now);
    let r = f(&mut ep);
    if !root
        .get("endpoints")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        root["endpoints"] = json!({});
    }
    root["endpoints"][&key] = endpoint_to_value(&ep);
    write_ledger(dir, &root);
    r
}

// --------------------------------------------------------------------------- #
// dir-level API (unit-testable without a Ctx) + Ctx wrappers
// --------------------------------------------------------------------------- #

/// The FLEET-wide runtime dir. ctx.runtime is Solomon/runtime/<name>; provider quotas are shared by
/// every lane, so the ledger lives one level up (Solomon/runtime/).
fn fleet_runtime_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ctx.control.join("runtime"))
}

fn preflight_at(dir: &Path, provider: &str, model: &str, now: u64) -> Decision {
    with_endpoint(dir, provider, model, now, |ep| decide(ep, now))
}

fn record_call_at(dir: &Path, provider: &str, model: &str, now: u64) {
    with_endpoint(dir, provider, model, now, |ep| {
        ep.spent_calls = ep.spent_calls.saturating_add(1);
    });
}

fn record_quota_at(dir: &Path, provider: &str, model: &str, now: u64) -> u64 {
    with_endpoint(dir, provider, model, now, |ep| {
        ep.consecutive_429 = ep.consecutive_429.saturating_add(1);
        ep.park_until = now + park_backoff_s(ep.consecutive_429);
        ep.park_until
    })
}

fn record_success_at(dir: &Path, provider: &str, model: &str, now: u64) {
    with_endpoint(dir, provider, model, now, |ep| {
        ep.consecutive_429 = 0;
    });
}

fn last_canary_pass_at(dir: &Path, provider: &str, model: &str) -> u64 {
    // read-only: atomic writes guarantee a whole file, so no lock is needed here
    read_ledger(dir)
        .get("endpoints")
        .and_then(|e| e.get(endpoint_key(provider, model)))
        .and_then(Value::as_object)
        .and_then(|o| u64_field(o, "last_canary_pass"))
        .unwrap_or(0)
}

/// May this endpoint be called right now? Parked when a 429 backoff is live or the weekly call
/// budget is spent. Seeds the endpoint on first sight (cap 500/week).
pub fn preflight(ctx: &Ctx, provider: &str, model: &str) -> Decision {
    preflight_at(&fleet_runtime_dir(ctx), provider, model, unix_now())
}

/// One real provider call is being spent NOW (called after a successful pi spawn, before waiting).
pub fn record_call(ctx: &Ctx, provider: &str, model: &str) {
    record_call_at(&fleet_runtime_dir(ctx), provider, model, unix_now());
}

/// A 429/quota outcome: bump consecutive_429 and park THIS endpoint (never the fleet) for
/// min(900*2^(n-1), 21600) seconds. Returns the park_until stamp.
pub fn record_quota(ctx: &Ctx, provider: &str, model: &str) -> u64 {
    record_quota_at(&fleet_runtime_dir(ctx), provider, model, unix_now())
}

/// A non-quota outcome: the endpoint answered, so the exponential 429 streak resets.
pub fn record_success(ctx: &Ctx, provider: &str, model: &str) {
    record_success_at(&fleet_runtime_dir(ctx), provider, model, unix_now());
}

/// Seconds since this endpoint last passed a canary; None when it never has.
pub fn last_canary_age_secs(ctx: &Ctx, provider: &str, model: &str) -> Option<u64> {
    let lcp = last_canary_pass_at(&fleet_runtime_dir(ctx), provider, model);
    if lcp == 0 {
        None
    } else {
        Some(unix_now().saturating_sub(lcp))
    }
}

/// The synthesized refusal stderr run_pi returns for a parked endpoint. The literal "429" is
/// load-bearing: pi::is_quota_error matches it, so iteration/oneshot/supervisor classify the
/// refusal as quota_error (never a model no-op) with zero new wiring.
pub fn parked_stderr(until: u64, why: &str) -> String {
    format!("429 provider parked by budget ledger until {until} — {why} (no token spent)")
}

// --------------------------------------------------------------------------- #
// effective endpoint selection (primary vs canaried fallback)
// --------------------------------------------------------------------------- #

/// Pure selection rule, injected canary for testability:
///   - primary healthy → primary;
///   - primary parked + no fallback / fallback==primary / fallback parked → primary (the caller's
///     preflight then refuses with the synthesized 429 — the fleet stays honest about being parked);
///   - primary parked + fallback with a canary pass within 24h → fallback;
///   - canary stale/never → run it ONCE, adopt only on pass (dead-fallback guard).
fn choose_endpoint(
    primary: (&str, &str),
    primary_parked: bool,
    fallback: Option<(String, String)>,
    fallback_parked: bool,
    fallback_canary_age: Option<u64>,
    canary: &mut dyn FnMut(&str, &str) -> bool,
) -> (String, String, bool) {
    let keep_primary = (primary.0.to_string(), primary.1.to_string(), false);
    if !primary_parked {
        return keep_primary;
    }
    let Some((fp, fm)) = fallback else {
        return keep_primary;
    };
    // catalog #2: a fallback must be a DIFFERENT endpoint (dotz's "fallback" was the same model)
    if fp == primary.0 && fm == primary.1 {
        return keep_primary;
    }
    // parking is per-endpoint; a parked fallback is no escape hatch
    if fallback_parked {
        return keep_primary;
    }
    match fallback_canary_age {
        Some(age) if age <= CANARY_FRESH_S => (fp, fm, true),
        _ => {
            if canary(&fp, &fm) {
                (fp, fm, true)
            } else {
                keep_primary
            }
        }
    }
}

/// Resolve the endpoint run_pi should call: the primary (ctx.pi_provider/ctx.pi_model — the same
/// fields run_pi's argv build reads), or the repo row's optional `"fallback": {"provider","model"}`
/// when the primary is parked, the fallback is not, and it passed a capability canary within 24h
/// (a stale canary is run lazily, once, right here — logged 'canarying fallback <ep>').
pub fn effective_endpoint(ctx: &mut Ctx) -> (String, String, bool) {
    let dir = fleet_runtime_dir(ctx);
    let now = unix_now();
    let primary = (ctx.pi_provider.clone(), ctx.pi_model.clone());
    if !preflight_at(&dir, &primary.0, &primary.1, now).is_parked() {
        return (primary.0, primary.1, false);
    }

    let name = ctx.name.clone();
    let row = gitops::repo_row(ctx, &name);
    let fallback = row
        .get("fallback")
        .and_then(Value::as_object)
        .and_then(|f| {
            let p = f.get("provider").and_then(Value::as_str).unwrap_or("").trim();
            let m = f.get("model").and_then(Value::as_str).unwrap_or("").trim();
            if p.is_empty() || m.is_empty() {
                None
            } else {
                Some((p.to_string(), m.to_string()))
            }
        });
    let (fallback_parked, fallback_canary_age) = match &fallback {
        Some((p, m)) => (
            preflight_at(&dir, p, m, now).is_parked(),
            match last_canary_pass_at(&dir, p, m) {
                0 => None,
                lcp => Some(now.saturating_sub(lcp)),
            },
        ),
        None => (false, None),
    };

    let mut canary = |p: &str, m: &str| -> bool {
        ctx.log(&format!("canarying fallback {p}:{m}"));
        run_canary(ctx, p, m)
    };
    choose_endpoint(
        (&primary.0, &primary.1),
        true, // reached only when the primary preflight above was Parked
        fallback,
        fallback_parked,
        fallback_canary_age,
        &mut canary,
    )
}

// --------------------------------------------------------------------------- #
// capability canary
// --------------------------------------------------------------------------- #

/// Filesystem-safe path component for the canary scratch dir (model ids carry '/' and ':').
fn fs_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// 8-hex nonce so a stale CANARY.txt (or a model echoing a remembered example) can never pass.
fn nonce8() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:08x}", (nanos ^ ((std::process::id() as u64) << 17)) as u32)
}

/// Exact-content check: the file must be the single line `CANARY-OK-<nonce>` (one trailing newline
/// tolerated — "a file containing exactly the single line X" is conventionally X + newline).
fn canary_content_ok(text: &str, nonce: &str) -> bool {
    let expected = format!("CANARY-OK-{nonce}");
    let t = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    t == expected
}

fn canary_file_ok(path: &Path, nonce: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|t| canary_content_ok(&t, nonce))
        .unwrap_or(false)
}

/// Bounded spawn for the canary: same pipe-drain + tree-kill discipline as run_pi (pi's node
/// grandchild holds the pipes open; a plain child-kill would hang the readers). None = spawn failed
/// (pi never started — no token spent, nothing to record).
fn run_command_bounded(mut cmd: Command, timeout_s: i64) -> Option<proc::RunOut> {
    use std::io::Read;
    use wait_timeout::ChildExt;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    pi::apply_spawn_flags(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let pid = child.id();
    let out_h = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let err_h = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let join = |h: Option<std::thread::JoinHandle<String>>| -> String {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    match child.wait_timeout(Duration::from_secs(timeout_s.max(0) as u64)) {
        Ok(Some(status)) => Some(proc::RunOut {
            code: status.code().unwrap_or(-1),
            stdout: join(out_h),
            stderr: join(err_h),
        }),
        Ok(None) => {
            pi::kill_tree(pid);
            let _ = child.wait_timeout(Duration::from_secs(20));
            let _ = child.kill();
            Some(proc::RunOut {
                code: 124,
                stdout: join(out_h),
                stderr: format!("canary timed out after {timeout_s}s"),
            })
        }
        Err(e) => {
            let _ = child.kill();
            drop(out_h);
            drop(err_h);
            Some(proc::RunOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            })
        }
    }
}

/// One tiny REAL implement-task against (provider, model), diff-verified: pass iff the agent
/// actually wrote CANARY.txt with the exact nonce line (narrated "success" with no file — the
/// nemotron failure shape — is a FAIL). Stamps last_canary_pass on pass. Runs in a scratch dir
/// under the fleet runtime (runtime/_canary/<provider>-<model>/), never in a managed repo.
pub fn run_canary(ctx: &mut Ctx, provider: &str, model: &str) -> bool {
    let dir = fleet_runtime_dir(ctx);
    let scratch = dir
        .join("_canary")
        .join(fs_safe(&format!("{provider}-{model}")));
    if std::fs::create_dir_all(&scratch).is_err() {
        return false; // no scratch dir => cannot diff-verify => not a pass
    }
    let canary_file = scratch.join("CANARY.txt");
    // a stale CANARY.txt must never fake a pass — the nonce already blocks content reuse, but
    // remove it so "file exists" is unambiguously THIS run's work
    let _ = std::fs::remove_file(&canary_file);
    let nonce = nonce8();
    let task = format!(
        "Create a file named CANARY.txt in the current directory containing exactly the single \
line CANARY-OK-{nonce}. Do not do anything else."
    );

    // Build the IDENTICAL argv run_pi would use (same batch-shim bypass, extensions, system
    // prompt) with THIS endpoint swapped in — a canary spawned any other way could pass while the
    // real call path stays broken (asmodeus's never-canaried dead fallback).
    let saved = (ctx.pi_provider.clone(), ctx.pi_model.clone());
    ctx.pi_provider = provider.to_string();
    ctx.pi_model = model.to_string();
    let args = pi::build_pi_argv(ctx, &task, None);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(&scratch);
    pi::apply_pi_env(ctx, &mut cmd);
    ctx.pi_provider = saved.0;
    ctx.pi_model = saved.1;

    let out = match run_command_bounded(cmd, CANARY_TIMEOUT_S) {
        Some(o) => o,
        None => {
            ctx.log(&format!("canary {provider}:{model} FAIL — pi did not spawn"));
            return false;
        }
    };
    let now = unix_now();
    record_call_at(&dir, provider, model, now); // the canary spent a real provider call
    if pi::is_quota_error_output(&out.stdout, &out.stderr) {
        let until = record_quota_at(&dir, provider, model, now);
        ctx.log(&format!(
            "canary {provider}:{model} hit provider quota — parked until {until}"
        ));
        return false;
    }
    let pass = canary_file_ok(&canary_file, &nonce);
    if pass {
        with_endpoint(&dir, provider, model, now, |ep| {
            ep.last_canary_pass = now;
            ep.consecutive_429 = 0;
        });
        ctx.log(&format!("canary {provider}:{model} PASS (diff-verified CANARY.txt)"));
    } else {
        ctx.log(&format!(
            "canary {provider}:{model} FAIL — CANARY.txt missing or wrong content (rc={})",
            out.code
        ));
    }
    pass
}

// --------------------------------------------------------------------------- #
// small helpers
// --------------------------------------------------------------------------- #

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn uniq() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("{:x}_{:x}", nanos, N.fetch_add(1, Ordering::Relaxed))
    }

    fn test_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("solomon_budget_test_{}", uniq()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// Isolated Ctx mirroring the freshness.rs test pattern: own control dir (repos.json) and a
    /// runtime dir whose PARENT is the fleet dir this suite writes ledgers into.
    fn test_ctx(repos_rows: Option<Value>) -> Ctx {
        let base = std::env::temp_dir().join(format!("solomon_budget_ctx_{}", uniq()));
        let control = base.join("control");
        let repo = base.join("repo");
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::create_dir_all(&repo);
        if let Some(rows) = repos_rows {
            std::fs::write(
                control.join("repos.json"),
                serde_json::to_string_pretty(&rows).unwrap(),
            )
            .unwrap();
        }
        let mut c = Ctx::configure(&repo.to_string_lossy(), "budgettest", "ollama-cloud", None);
        c.control = control;
        c.runtime = base.join("runtime").join("budgettest");
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    fn read_ep(dir: &Path, provider: &str, model: &str) -> Value {
        read_ledger(dir)["endpoints"][endpoint_key(provider, model)].clone()
    }

    fn seed_ep(dir: &Path, provider: &str, model: &str, ep: Value) {
        let mut root = read_ledger(dir);
        if !root.get("endpoints").map(Value::is_object).unwrap_or(false) {
            root["endpoints"] = json!({});
        }
        root["endpoints"][endpoint_key(provider, model)] = ep;
        write_ledger(dir, &root);
    }

    // ---- exponential park schedule: 900/1800/3600/.../21600 cap ----

    #[test]
    fn park_backoff_schedule_is_exponential_with_6h_cap() {
        assert_eq!(park_backoff_s(1), 900);
        assert_eq!(park_backoff_s(2), 1800);
        assert_eq!(park_backoff_s(3), 3600);
        assert_eq!(park_backoff_s(4), 7200);
        assert_eq!(park_backoff_s(5), 14400);
        assert_eq!(park_backoff_s(6), 21600, "6th hits the 6h cap exactly");
        assert_eq!(park_backoff_s(7), 21600, "cap holds forever after");
        assert_eq!(park_backoff_s(64), 21600, "shift clamp: no overflow at absurd counts");
        // explicitly NOT the Gen-2 86400s blanket
        assert!(park_backoff_s(64) < 86_400);
    }

    #[test]
    fn record_quota_parks_with_exponential_schedule_and_success_resets() {
        let dir = test_dir();
        let now = unix_now();
        assert_eq!(record_quota_at(&dir, "p", "m", now), now + 900);
        assert_eq!(record_quota_at(&dir, "p", "m", now), now + 1800);
        assert_eq!(record_quota_at(&dir, "p", "m", now), now + 3600);
        assert_eq!(read_ep(&dir, "p", "m")["consecutive_429"], json!(3));
        // parked now, but proceed once the park expires
        assert!(preflight_at(&dir, "p", "m", now).is_parked());
        assert!(!preflight_at(&dir, "p", "m", now + 3601).is_parked());
        // a success resets the streak: the NEXT quota error starts back at 15min
        record_success_at(&dir, "p", "m", now + 3601);
        assert_eq!(read_ep(&dir, "p", "m")["consecutive_429"], json!(0));
        assert_eq!(record_quota_at(&dir, "p", "m", now + 3700), now + 3700 + 900);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- per-endpoint isolation: parking A leaves B Proceed ----

    #[test]
    fn parking_endpoint_a_leaves_endpoint_b_proceed() {
        let dir = test_dir();
        let now = unix_now();
        record_quota_at(&dir, "maki-cloud", "glm-5.2", now);
        assert!(preflight_at(&dir, "maki-cloud", "glm-5.2", now).is_parked());
        // same provider, different model: independent budget row
        assert_eq!(
            preflight_at(&dir, "maki-cloud", "minimax-m3", now),
            Decision::Proceed
        );
        // different provider entirely
        assert_eq!(
            preflight_at(&dir, "openrouter", "qwen/qwen3-coder", now),
            Decision::Proceed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- window rollover ----

    #[test]
    fn window_rollover_resets_spent_after_a_week() {
        let dir = test_dir();
        let t0 = unix_now();
        record_call_at(&dir, "p", "m", t0);
        record_call_at(&dir, "p", "m", t0);
        record_call_at(&dir, "p", "m", t0);
        assert_eq!(read_ep(&dir, "p", "m")["spent_calls"], json!(3));
        // now - window_started > 604800 => new window, spent reset
        let later = t0 + WEEK_S + 1;
        assert_eq!(preflight_at(&dir, "p", "m", later), Decision::Proceed);
        let ep = read_ep(&dir, "p", "m");
        assert_eq!(ep["spent_calls"], json!(0));
        assert_eq!(ep["window_started"], json!(later));
        // exactly AT the boundary (== 604800) the old window still stands
        let dir2 = test_dir();
        record_call_at(&dir2, "p", "m", t0);
        preflight_at(&dir2, "p", "m", t0 + WEEK_S);
        assert_eq!(read_ep(&dir2, "p", "m")["spent_calls"], json!(1));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn rollover_unparks_a_cap_exhausted_endpoint_but_not_a_429_park() {
        let dir = test_dir();
        let t0 = unix_now();
        seed_ep(
            &dir,
            "p",
            "m",
            json!({"window_cap_calls": 5, "window_started": t0, "spent_calls": 5,
                   "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0}),
        );
        assert!(preflight_at(&dir, "p", "m", t0).is_parked(), "cap spent -> parked");
        assert!(
            !preflight_at(&dir, "p", "m", t0 + WEEK_S + 1).is_parked(),
            "new window -> spend again"
        );
        // a live 429 park survives rollover (backoff is about the provider, not the window)
        let t1 = unix_now();
        seed_ep(
            &dir,
            "q",
            "m",
            json!({"window_cap_calls": 500, "window_started": t1 - WEEK_S - 10, "spent_calls": 400,
                   "park_until": t1 + 600, "consecutive_429": 1, "last_canary_pass": 0}),
        );
        assert!(preflight_at(&dir, "q", "m", t1).is_parked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- cap exhausted => Parked (reserve headroom) ----

    #[test]
    fn cap_exhausted_is_parked_until_window_end() {
        let dir = test_dir();
        let t0 = unix_now();
        seed_ep(
            &dir,
            "p",
            "m",
            json!({"window_cap_calls": 500, "window_started": t0, "spent_calls": 500,
                   "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0}),
        );
        match preflight_at(&dir, "p", "m", t0) {
            Decision::Parked { until, why } => {
                assert_eq!(until, t0 + WEEK_S, "parked until the window rolls");
                assert!(why.contains("cap exhausted"), "got: {why}");
                assert!(why.contains("500/500"), "got: {why}");
            }
            Decision::Proceed => panic!("spent >= cap must be Parked (never plan past the cap)"),
        }
        // one call of headroom left: the cap-th call may still be planned
        seed_ep(
            &dir,
            "p",
            "m2",
            json!({"window_cap_calls": 500, "window_started": t0, "spent_calls": 499,
                   "park_until": 0, "consecutive_429": 0, "last_canary_pass": 0}),
        );
        assert_eq!(preflight_at(&dir, "p", "m2", t0), Decision::Proceed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preflight_seeds_endpoint_on_first_sight_with_default_cap() {
        let dir = test_dir();
        let now = unix_now();
        assert_eq!(preflight_at(&dir, "new-prov", "new-model", now), Decision::Proceed);
        let ep = read_ep(&dir, "new-prov", "new-model");
        assert_eq!(ep["window_cap_calls"], json!(500));
        assert_eq!(ep["spent_calls"], json!(0));
        assert_eq!(ep["park_until"], json!(0));
        assert_eq!(ep["consecutive_429"], json!(0));
        assert_eq!(ep["last_canary_pass"], json!(0));
        assert!(ep["window_started"].as_u64().unwrap() >= now);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- choose_endpoint: fallback selection incl. stale-canary refusal ----

    #[test]
    fn choose_endpoint_matrix() {
        let prim = ("maki-cloud", "glm-5.2");
        let fb = || Some(("openrouter".to_string(), "nemo".to_string()));
        let mut no_canary = |_: &str, _: &str| -> bool { panic!("canary must not run here") };

        // primary healthy -> primary, even with a shiny fallback configured
        assert_eq!(
            choose_endpoint(prim, false, fb(), false, Some(10), &mut no_canary),
            ("maki-cloud".to_string(), "glm-5.2".to_string(), false)
        );
        // parked, no fallback -> primary (caller's preflight then refuses honestly)
        assert_eq!(
            choose_endpoint(prim, true, None, false, None, &mut no_canary),
            ("maki-cloud".to_string(), "glm-5.2".to_string(), false)
        );
        // parked, fallback == primary -> primary (a fallback must be a DIFFERENT endpoint)
        assert_eq!(
            choose_endpoint(
                prim,
                true,
                Some(("maki-cloud".to_string(), "glm-5.2".to_string())),
                false,
                Some(10),
                &mut no_canary
            ),
            ("maki-cloud".to_string(), "glm-5.2".to_string(), false)
        );
        // parked, fallback itself parked -> primary (parking is per-endpoint, no cascade)
        assert_eq!(
            choose_endpoint(prim, true, fb(), true, Some(10), &mut no_canary),
            ("maki-cloud".to_string(), "glm-5.2".to_string(), false)
        );
        // parked, fallback canaried within 24h -> fallback
        assert_eq!(
            choose_endpoint(prim, true, fb(), false, Some(CANARY_FRESH_S), &mut no_canary),
            ("openrouter".to_string(), "nemo".to_string(), true)
        );
    }

    #[test]
    fn choose_endpoint_stale_canary_runs_once_and_adopt_only_on_pass() {
        let prim = ("maki-cloud", "glm-5.2");
        let fb = || Some(("openrouter".to_string(), "nemo".to_string()));

        // stale (age > 24h): canary runs; FAIL -> refuse the fallback, keep primary
        let mut calls = 0;
        let mut failing = |p: &str, m: &str| -> bool {
            calls += 1;
            assert_eq!((p, m), ("openrouter", "nemo"));
            false
        };
        assert_eq!(
            choose_endpoint(prim, true, fb(), false, Some(CANARY_FRESH_S + 1), &mut failing),
            ("maki-cloud".to_string(), "glm-5.2".to_string(), false),
            "an uncanaried fallback must NOT be adopted (dead-fallback guard)"
        );
        assert_eq!(calls, 1, "the lazy canary runs exactly once");

        // never canaried (None): canary runs; PASS -> adopt
        let mut passing = |_: &str, _: &str| -> bool { true };
        assert_eq!(
            choose_endpoint(prim, true, fb(), false, None, &mut passing),
            ("openrouter".to_string(), "nemo".to_string(), true)
        );
    }

    #[test]
    fn effective_endpoint_selects_canaried_fallback_when_primary_parked() {
        // fs-integrated: primary parked in the ledger, fallback configured in repos.json with a
        // FRESH canary stamp -> the fallback is chosen without spawning anything.
        let rows = json!([{
            "name": "budgettest",
            "fallback": {"provider": "openrouter", "model": "nvidia/nemotron-3-ultra-550b-a55b:free"}
        }]);
        let mut c = test_ctx(Some(rows));
        let dir = fleet_runtime_dir(&c);
        let now = unix_now();
        // ctx defaults: provider maki-cloud, model glm-5.2 (ollama-cloud row)
        record_quota_at(&dir, &c.pi_provider.clone(), &c.pi_model.clone(), now); // parks primary 900s
        seed_ep(
            &dir,
            "openrouter",
            "nvidia/nemotron-3-ultra-550b-a55b:free",
            json!({"window_cap_calls": 500, "window_started": now, "spent_calls": 0,
                   "park_until": 0, "consecutive_429": 0, "last_canary_pass": now - 100}),
        );
        let (p, m, is_fb) = effective_endpoint(&mut c);
        assert_eq!(
            (p.as_str(), m.as_str(), is_fb),
            ("openrouter", "nvidia/nemotron-3-ultra-550b-a55b:free", true)
        );
        // and the ctx's own configured endpoint is untouched by resolution
        assert_eq!(c.pi_provider, "maki-cloud");
        assert_eq!(c.pi_model, "glm-5.2");
    }

    #[test]
    fn effective_endpoint_keeps_primary_when_healthy_or_fallback_unusable() {
        // healthy primary -> primary
        let rows = json!([{
            "name": "budgettest",
            "fallback": {"provider": "openrouter", "model": "x"}
        }]);
        let mut c = test_ctx(Some(rows));
        let (p, m, is_fb) = effective_endpoint(&mut c);
        assert_eq!((p, m, is_fb), (c.pi_provider.clone(), c.pi_model.clone(), false));

        // parked primary + parked fallback -> primary (run_pi's preflight then refuses honestly)
        let rows = json!([{
            "name": "budgettest",
            "fallback": {"provider": "openrouter", "model": "x"}
        }]);
        let mut c = test_ctx(Some(rows));
        let dir = fleet_runtime_dir(&c);
        let now = unix_now();
        record_quota_at(&dir, &c.pi_provider.clone(), &c.pi_model.clone(), now);
        record_quota_at(&dir, "openrouter", "x", now);
        let (_, _, is_fb) = effective_endpoint(&mut c);
        assert!(!is_fb, "a parked fallback is no escape hatch");

        // parked primary + NO fallback key -> primary, and no canary attempted
        let mut c = test_ctx(Some(json!([{ "name": "budgettest" }])));
        let dir = fleet_runtime_dir(&c);
        record_quota_at(&dir, &c.pi_provider.clone(), &c.pi_model.clone(), unix_now());
        let (p, m, is_fb) = effective_endpoint(&mut c);
        assert_eq!((p, m, is_fb), (c.pi_provider.clone(), c.pi_model.clone(), false));
    }

    // ---- canary content verification (manual file, wrong nonce) ----

    #[test]
    fn canary_content_verification_pass_and_fail() {
        let dir = test_dir();
        let path = dir.join("CANARY.txt");
        // exact single line -> pass; one trailing newline (either flavor) tolerated
        std::fs::write(&path, "CANARY-OK-1a2b3c4d").unwrap();
        assert!(canary_file_ok(&path, "1a2b3c4d"));
        std::fs::write(&path, "CANARY-OK-1a2b3c4d\n").unwrap();
        assert!(canary_file_ok(&path, "1a2b3c4d"));
        std::fs::write(&path, "CANARY-OK-1a2b3c4d\r\n").unwrap();
        assert!(canary_file_ok(&path, "1a2b3c4d"));
        // wrong nonce -> fail (a stale file from an earlier canary can never pass)
        std::fs::write(&path, "CANARY-OK-deadbeef").unwrap();
        assert!(!canary_file_ok(&path, "1a2b3c4d"));
        // narration around the line -> fail (diff-verified means EXACT content)
        std::fs::write(&path, "I created the file!\nCANARY-OK-1a2b3c4d\n").unwrap();
        assert!(!canary_file_ok(&path, "1a2b3c4d"));
        std::fs::write(&path, "CANARY-OK-1a2b3c4d\n\n").unwrap();
        assert!(!canary_file_ok(&path, "1a2b3c4d"), "two trailing newlines is not the single line");
        // missing file -> fail
        assert!(!canary_file_ok(&dir.join("NOPE.txt"), "1a2b3c4d"));
        // nonce shape: 8 hex chars
        let n = nonce8();
        assert_eq!(n.len(), 8);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- lock contention: second writer waits; stale lock broken ----

    #[test]
    fn lock_second_writer_waits_and_breaks_stale_locks() {
        let dir = test_dir();
        // held lock: a second writer times out (bounded wait, never forever)
        let g = acquire_lock(&dir, Duration::from_millis(100), Duration::from_secs(30))
            .expect("first writer acquires");
        assert!(
            acquire_lock(&dir, Duration::from_millis(150), Duration::from_secs(30)).is_none(),
            "second writer must give up after its bounded wait while the lock is held"
        );
        drop(g); // release
        let g2 = acquire_lock(&dir, Duration::from_millis(100), Duration::from_secs(30));
        assert!(g2.is_some(), "released lock is immediately acquirable");
        drop(g2);
        // stale lock: a leftover lockfile older than the threshold is broken, not waited on
        std::fs::write(dir.join(LOCK_NAME), "dead-holder").unwrap();
        let g3 = acquire_lock(&dir, Duration::from_millis(400), Duration::from_secs(0));
        assert!(g3.is_some(), "stale (0s threshold) lock must be broken and re-acquired");
        drop(g3);
        assert!(!dir.join(LOCK_NAME).exists(), "guard drop removes the lockfile");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_serializes_concurrent_spend_counting() {
        let dir = test_dir();
        let now = unix_now();
        let d1 = dir.clone();
        let d2 = dir.clone();
        let t1 = std::thread::spawn(move || {
            for _ in 0..10 {
                record_call_at(&d1, "p", "m", now);
            }
        });
        let t2 = std::thread::spawn(move || {
            for _ in 0..10 {
                record_call_at(&d2, "p", "m", now);
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(
            read_ep(&dir, "p", "m")["spent_calls"],
            json!(20),
            "locked read-modify-write must not lose increments"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- synthesized-parked stderr matches pi::is_quota_error ----

    #[test]
    fn parked_stderr_is_classified_as_quota_error_by_existing_paths() {
        let s = parked_stderr(1_751_900_000, "endpoint parked after 2 consecutive 429(s)");
        assert!(
            pi::is_quota_error(&s),
            "the synthesized refusal must ride the existing quota classification: {s}"
        );
        assert!(
            pi::is_quota_error_output("", &s),
            "output-level classifier (stdout empty, stderr synthesized) must also match"
        );
        assert!(s.contains("no token spent"));
        assert!(s.contains("until 1751900000"));
    }

    // ---- ledger resilience ----

    #[test]
    fn torn_or_missing_ledger_degrades_to_empty_and_reseeds() {
        let dir = test_dir();
        std::fs::write(ledger_path(&dir), "{ not json").unwrap();
        let now = unix_now();
        assert_eq!(preflight_at(&dir, "p", "m", now), Decision::Proceed);
        // the write path healed the file into a valid ledger
        let v = read_ledger(&dir);
        assert!(v["endpoints"][endpoint_key("p", "m")].is_object());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
