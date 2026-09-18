//! CEO autonomy, piece 1 — the COLD-OUTREACH pipeline: gated cold-email drafts that AUTO-SEND
//! behind AUTOMATED guardrails (no human approval), when SMTP env creds are present.
//!
//! ============================ WHAT THIS ADDS =============================
//! The operator's end goal names "cold outreach" as a CEO capability, but the growth plane only
//! drafts lane-branded CONTENT. This module adds the missing outreach lane, reusing the growth
//! machinery wholesale: the honest planner-directive trigger, the public-lane eligibility
//! predicate, the persona deny filter, the dated-fact drafts log. Targets are OPERATOR-SUPPLIED
//! ONLY (`runtime/<lane>/outreach_targets.json`): Solomon never scrapes, guesses, or enriches
//! contact data — an absent/empty target list yields an empty template + one deduped needs card,
//! never a fabricated contact.
//! ========================================================================
//!
//! ## Why this is safe by CONSTRUCTION (AUTOMATED safety, not a human gate)
//!   * COMPOSE writes a GATED, provenance-tagged draft line to `outreach_drafts.jsonl` AND the FULL
//!     sendable payload to `outreach_outbox.jsonl` — local appends under Solomon's runtime dir,
//!     day-capped (`outreach.max_drafts_per_day`, default 3), deduped per target forever,
//!     budget-aware (`fleet::daily_calls_remaining`), persona-filtered.
//!   * SEND is AUTONOMOUS behind AUTOMATED guardrails ONLY: it ships the oldest UNSENT outbox entry
//!     when (the recipient is on the OPERATOR-SUPPLIED target list — the hard anti-scrape line),
//!     AND SMTP env creds exist, AND the sover-grade rate caps (5/day, 1/hour defaults) allow, AND
//!     the persona filter re-passes, AND an honest sender-identity/opt-out footer is attached.
//!     Idempotent by message signature — a message sends at most once, ever, across any number of
//!     sweeps and OS processes. There is NO `approved:true` wait.
//!   * TRANSPORT is `curl.exe` SMTP (the exact zero-dependency posture `notify.rs` established) —
//!     no lettre/reqwest/tokio; the argv is a pure, pinned-by-test function.
//!   * Every artifact lives under gitignored `runtime/` — no new tracked file, so the provenance
//!     tripwire (`provenance::TRACKED`) is untouched.
//!
//! ## HARD INVARIANTS (never violated)
//! Recipients are OPERATOR-SUPPLIED ONLY (never fabricated/scraped — re-checked at send). No SMTP
//! creds => no send (+ one deduped needs card) — a DATA dependency, the only thing that keeps the
//! seam inert. A send failure never fakes success (the signature is recorded ONLY on curl exit 0,
//! so a transient failure stays retriable). Rate caps + persona + the honest footer are AUTOMATED
//! safety and stay.
#![allow(dead_code)]

use crate::control::{paths, proc};
use crate::notify::{self, Notice};
use crate::pecrt::warm::ObservationLog;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

// Reuse — never fork — the growth plane's deterministic persona deny filter (§ the two-copies-drift
// trap): the same operator-marker gate the growth composer enforces.
pub(crate) use crate::ceo::growth::violates_persona;

// --------------------------------------------------------------------------- //
// config + constants
// --------------------------------------------------------------------------- //

/// Default per-lane caps (sover's investor-grade email ceilings, `email_outreach.rs` precedent):
/// 3 composed drafts/day, 5 sends/day, 1 send/hour. Overridable per lane via a repos.json
/// `outreach` block — ABSENT block = these safe defaults (there is no `enabled` flag to forget:
/// drafting is harmless + gated, sending is double-gated by approval + SMTP presence).
const DEFAULT_MAX_DRAFTS_PER_DAY: i64 = 3;
const DEFAULT_DAILY_SEND_CAP: i64 = 5;
const DEFAULT_HOURLY_SEND_CAP: i64 = 1;

/// curl SMTP wall-clock ceiling: `-m 30` on the argv + spawn slack on the wait.
const SEND_TIMEOUT_S: u64 = 35;

/// Keywords that mark a planner backlog line as a COLD-OUTREACH directive. Deliberately heuristic
/// (the `growth::GROWTH_DIRECTIVE_KEYWORDS` risk posture): a false negative is a quiet day (safe);
/// a false positive is at worst one gated local draft behind the gates (safe).
const OUTREACH_DIRECTIVE_KEYWORDS: &[&str] = &[
    "outreach",
    "cold email",
    "cold-email",
    "cold outreach",
    "email campaign",
    "reach out",
    "contact list",
    "partnership email",
    "pitch email",
];

/// The outreach writer's role contract: evidence-grounded, persona-safe, non-spammy, STRICT JSON
/// out. The EVIDENCE and PERSONA rules are stated here AND enforced deterministically after the
/// reply (`violates_persona`) — prompt-level alone is a wish, the filter is the gate.
const OUTREACH_COMPOSER_PROMPT: &str = "You are the outreach writer for ONE software project. \
    Given the project brand (lane name), its north-star goal, its measured 24h outcomes, and ONE \
    operator-vetted target contact (name, org, and the operator's rationale for why they fit), \
    draft ONE short, honest, non-spammy cold-outreach email. EVIDENCE RULE: every claim is \
    grounded in the measured outcomes provided — never invent traction, users, funding, or \
    endorsements. PERSONA RULE: author under the project's own brand, never the operator's \
    personal name, handle, or email. No paid offers, no attachments, no tracking links. 3-6 \
    sentences, plain text, first person as the project. Reply STRICT JSON only: \
    {\"subject\":\"<one line>\",\"body\":\"<plain-text body>\"}";

// --------------------------------------------------------------------------- //
// paths (all under runtime/<lane>/ — gitignored, janitor-bounded, never tracked)
// --------------------------------------------------------------------------- //

/// `runtime/<lane>/outreach_targets.json` — the OPERATOR-OWNED target list. None for a nameless row.
fn targets_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("outreach_targets.json"))
}

/// `runtime/<lane>/outreach_drafts.jsonl` — the gated drafts log (sibling of growth_drafts.jsonl,
/// same `ObservationLog` mechanism + janitor rotation).
fn drafts_log_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("outreach_drafts.jsonl"))
}

/// `runtime/<lane>/_outreach_drafted.json` — per-lane dedup + day-cap state (inert marker class).
fn drafted_state_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("_outreach_drafted.json"))
}

/// `runtime/<lane>/_outreach_sent.json` — per-lane send idempotency + rate state.
fn sent_state_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("_outreach_sent.json"))
}

/// `runtime/<lane>/outreach_sent.jsonl` — append-only audit trail of ACTUAL send attempts.
fn sent_log_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("outreach_sent.jsonl"))
}

/// `runtime/<lane>/outreach_outbox.jsonl` — the AUTONOMOUS send queue. The composer appends the FULL
/// sendable payload (`{ts,target_key,to,subject,body,signature}`) here on a clean draft; the
/// auto-send seam reads the oldest UNSENT entry. Distinct from `outreach_drafts.jsonl` (the
/// human-readable provenance log, which stores only a truncated body head) so autonomous sending has
/// the complete message without a human re-typing it. None for a nameless row.
fn outbox_path(repo: &Value) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join("outreach_outbox.jsonl"))
}

/// `runtime/<lane>/_outreach_needs_paged_<which>` — the needs-card dedupe marker (`which` is
/// `targets` or `smtp`; the two cards have different remedies so each dedupes independently —
/// mirrors provenance.rs's `_config_drift_paged_<file>` per-file markers, catalog #8 page-flood).
fn needs_marker_path(repo: &Value, which: &str) -> Option<PathBuf> {
    paths::runtime_dir(repo).map(|d| d.join(format!("_outreach_needs_paged_{which}")))
}

// --------------------------------------------------------------------------- //
// pure helpers (unit-tested)
// --------------------------------------------------------------------------- //

/// First 8 hex chars of the vendored SHA-256 — the target dedup key / prompt-hash shape.
fn sha8(s: &str) -> String {
    crate::provenance::sha256_hex(s.as_bytes())[..8].to_string()
}

/// True iff a backlog line reads as a cold-outreach directive (keyword heuristic, pure).
pub(crate) fn is_outreach_directive(line: &str) -> bool {
    let lower = line.to_lowercase();
    OUTREACH_DIRECTIVE_KEYWORDS
        .iter()
        .any(|k| lower.contains(k))
}

/// Find TODAY'S planner-composed OUTREACH directive in a lane's backlog (pure — the exact
/// open/non-deferred/today-or-campaign shape `growth::growth_directive` uses, with the outreach
/// keyword set). None = a quiet day; this is the HONEST trigger, never a static daily fact.
pub(crate) fn outreach_directive(backlog: &str, today_marker: &str) -> Option<String> {
    backlog
        .lines()
        .map(str::trim)
        .find(|l| {
            l.starts_with("- [ ]")
                && !l.contains("(deferred")
                && (l.contains(today_marker) || l.contains("[campaign:"))
                && is_outreach_directive(l)
        })
        .map(|l| l.trim_start_matches("- [ ]").trim().to_string())
}

/// The stable per-target dedup key: sha8 of the lowercased, trimmed email (pure).
pub(crate) fn target_key(email: &str) -> String {
    sha8(&email.trim().to_lowercase())
}

/// Parse the operator's targets file body and keep only HONEST entries: a target missing `email`
/// or `rationale` is dropped (no rationale => no draft — Solomon never invents the "why"). Pure.
pub(crate) fn load_targets(raw: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("targets").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|t| {
            let has = |k: &str| {
                t.get(k)
                    .and_then(Value::as_str)
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
            };
            has("email") && has("rationale")
        })
        .collect()
}

/// Per-lane caps from the optional repos.json `outreach` block (pure):
/// `(max_drafts_per_day, daily_send_cap, hourly_send_cap)`, safe defaults when absent/invalid.
pub(crate) fn caps(repo: &Value) -> (i64, i64, i64) {
    let block = repo.get("outreach").cloned().unwrap_or_else(|| json!({}));
    let get = |k: &str, d: i64| {
        block
            .get(k)
            .and_then(Value::as_i64)
            .filter(|v| *v >= 0)
            .unwrap_or(d)
    };
    (
        get("max_drafts_per_day", DEFAULT_MAX_DRAFTS_PER_DAY),
        get("daily_send_cap", DEFAULT_DAILY_SEND_CAP),
        get("hourly_send_cap", DEFAULT_HOURLY_SEND_CAP),
    )
}

/// Build the provenance-tagged DRAFT fact line (pure — caller supplies the epoch). Carries the
/// full provenance the task demands: model id, prompt hash, target dedup key, recipient, and the
/// operator's rationale — one dated first-order line (the `t=` datum satisfies
/// `ObservationLog::validate_fact`).
pub(crate) fn draft_fact(
    model: &str,
    prompt_hash: &str,
    lane: &str,
    target: &Value,
    subject: &str,
    body: &str,
    epoch_s: u64,
) -> String {
    let email = target.get("email").and_then(Value::as_str).unwrap_or("");
    let key = target_key(email);
    let subj = super::cap_line(subject, 120);
    let body_head = super::cap_line(body, 160);
    let rationale = super::cap_line(
        target
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or(""),
        120,
    );
    format!(
        "rsi: outreach DRAFT [GATED, unsent, cold-email] lane={lane} target={key} to={email} \
         subj={subj} :: {body_head} (t={epoch_s} model={model} prompt={prompt_hash} \
         rationale={rationale})"
    )
}

/// Parse the composer's STRICT-JSON reply into (subject, body) — via the same `extract_json` +
/// `cap_line` seams the growth composer uses. A blank subject OR body is None (a blank cold email
/// is the busywork the eval-park doctrine forbids — fail-closed, nothing drafted). Pure.
pub(crate) fn parse_outreach_reply(reply: &str) -> Option<(String, String)> {
    let parsed = super::extract_json(reply)?;
    let subject = super::cap_line(
        parsed.get("subject").and_then(Value::as_str).unwrap_or(""),
        120,
    );
    let body = super::cap_line(
        parsed.get("body").and_then(Value::as_str).unwrap_or(""),
        800,
    );
    if subject.is_empty() || body.is_empty() {
        return None;
    }
    Some((subject, body))
}

/// True iff a send is under the daily + hourly caps given the rate state (pure over the injected
/// clock — port of sover `email_outreach::rate_ok`). A stale `date` resets everything.
pub(crate) fn rate_ok_at(
    st: &Value,
    daily_cap: i64,
    hourly_cap: i64,
    today: &str,
    hour: i64,
) -> bool {
    if st.get("date").and_then(Value::as_str) != Some(today) {
        return true;
    }
    let count = st.get("count").and_then(Value::as_i64).unwrap_or(0);
    if daily_cap != 0 && count >= daily_cap {
        return false;
    }
    let st_hour = st.get("hour").and_then(Value::as_i64);
    let hour_count = st.get("hour_count").and_then(Value::as_i64).unwrap_or(0);
    if hourly_cap != 0 && st_hour == Some(hour) && hour_count >= hourly_cap {
        return false;
    }
    true
}

/// Bump the rate state after a SUCCESSFUL send (pure over the injected clock — port of sover
/// `email_outreach::bump_rate`). A new day resets the counters before bumping.
pub(crate) fn bump_rate_at(st: &mut Value, today: &str, hour: i64) {
    if st.get("date").and_then(Value::as_str) != Some(today) {
        st["date"] = json!(today);
        st["count"] = json!(0);
        st["hour"] = json!(-1);
        st["hour_count"] = json!(0);
    }
    let count = st.get("count").and_then(Value::as_i64).unwrap_or(0) + 1;
    let hour_count = if st.get("hour").and_then(Value::as_i64) == Some(hour) {
        st.get("hour_count").and_then(Value::as_i64).unwrap_or(0) + 1
    } else {
        1
    };
    st["count"] = json!(count);
    st["hour_count"] = json!(hour_count);
    st["hour"] = json!(hour);
}

fn local_today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn local_hour() -> i64 {
    use chrono::Timelike;
    chrono::Local::now().hour() as i64
}

fn rate_ok(st: &Value, daily_cap: i64, hourly_cap: i64) -> bool {
    rate_ok_at(st, daily_cap, hourly_cap, &local_today(), local_hour())
}

fn bump_rate(st: &mut Value) {
    bump_rate_at(st, &local_today(), local_hour());
}

/// The stable per-message send-idempotency signature: SHA-256 over `to\nsubject\nbody` (pure). An
/// already-sent signature is never re-sent, so re-composing the identical message is a no-op.
pub(crate) fn send_signature(to: &str, subject: &str, body: &str) -> String {
    crate::provenance::sha256_hex(format!("{}\n{}\n{}", to.trim(), subject.trim(), body).as_bytes())
}

/// Append a minimal, TRUTHFUL sender-identity + opt-out footer to a cold-email body when one is not
/// already present — the honesty floor for AUTONOMOUS sending (CAN-SPAM-style: identify the sender +
/// offer an opt-out). The message is identified as the PROJECT's automated outreach (the lane brand,
/// never the operator's name — persona-safe) and is a one-time note (the composer dedupes each target
/// forever, so this is literally true). Idempotent: a body that already carries an unsubscribe /
/// one-time-note line is returned unchanged. Pure — unit-tested.
pub(crate) fn ensure_footer(body: &str, lane: &str) -> String {
    let lower = body.to_lowercase();
    if lower.contains("unsubscribe") || lower.contains("one-time note") {
        return body.to_string();
    }
    format!(
        "{body}\n\n\u{2014}\nSent by the {lane} project's automated outreach (a one-time note). \
         Reply if you'd prefer we don't follow up."
    )
}

/// Read the outbox (`outreach_outbox.jsonl`) as parsed records, skipping any unparseable line (a
/// corrupt line can never wedge the queue). Newest last, matching append order. Pure over the file.
fn read_outbox(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The idempotency key for an outbox entry: its recorded `signature` when present (the composer
/// stamps one), else derived from `to/subject/body` — so the scan filter and the send dedup can
/// never disagree. Pure.
fn outbox_signature(e: &Value) -> String {
    if let Some(s) = e.get("signature").and_then(Value::as_str) {
        return s.to_string();
    }
    send_signature(
        e.get("to").and_then(Value::as_str).unwrap_or("").trim(),
        e.get("subject").and_then(Value::as_str).unwrap_or(""),
        e.get("body").and_then(Value::as_str).unwrap_or(""),
    )
}

// --------------------------------------------------------------------------- //
// SMTP creds (env, fail-closed) + the pure curl argv
// --------------------------------------------------------------------------- //

/// SMTP transport credentials, resolved fail-closed: absent HOST/USER/PASS => NO send path exists.
pub(crate) struct SmtpCreds {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub from: String,
}

/// Read a key from the process env first, then `<HERE>/.env` (notify's precedence). Empty = absent.
fn env_or_dotenv(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| notify::env_value(key))
}

/// Resolve `SOLOMON_SMTP_*` creds. None whenever HOST, USER, or PASS is absent — the send seam
/// then pages the needs card ONCE and leaves the approved draft unsent (fail-closed, edge E8).
/// PORT defaults 465 (smtps); FROM defaults to USER.
fn smtp_creds() -> Option<SmtpCreds> {
    let host = env_or_dotenv("SOLOMON_SMTP_HOST")?;
    let user = env_or_dotenv("SOLOMON_SMTP_USER")?;
    let pass = env_or_dotenv("SOLOMON_SMTP_PASS")?;
    let port = env_or_dotenv("SOLOMON_SMTP_PORT")
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(465);
    let from = env_or_dotenv("SOLOMON_SMTP_FROM").unwrap_or_else(|| user.clone());
    Some(SmtpCreds {
        host,
        port,
        user,
        pass,
        from,
    })
}

/// The curl SMTP argv (pure — the `#[test]` pins the shape). Port 587 selects the STARTTLS form
/// (`smtp://` + `--ssl-reqd` upgrades the session); any other port (465 default) is implicit-TLS
/// `smtps://`. `curl.exe` ships with Windows 10+ — the same assumption notify.rs already makes.
pub(crate) fn curl_smtp_argv(c: &SmtpCreds, eml_path: &str, to: &str) -> Vec<String> {
    let url = if c.port == 587 {
        format!("smtp://{}:{}", c.host, c.port)
    } else {
        format!("smtps://{}:{}", c.host, c.port)
    };
    vec![
        "curl".into(),
        "-s".into(),
        "-m".into(),
        "30".into(),
        "--ssl-reqd".into(),
        "--url".into(),
        url,
        "--mail-from".into(),
        c.from.clone(),
        "--mail-rcpt".into(),
        to.into(),
        "--upload-file".into(),
        eml_path.into(),
        "--user".into(),
        format!("{}:{}", c.user, c.pass),
    ]
}

/// RAII cleanup for the temp .eml so the message body never lingers under runtime/ after the send
/// returns — every exit path (ok, non-zero, spawn error) removes it (the ceo.rs TempFileGuard
/// discipline).
struct TempEmlGuard(PathBuf);
impl Drop for TempEmlGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Send ONE message over curl SMTP: write a minimal RFC-822 .eml under the lane's runtime dir
/// (guarded — removed on every exit path), then run the pinned argv windowless with a bounded
/// wait. Headers are single-line-flattened upstream, so header injection cannot occur here.
fn send_via_curl(
    c: &SmtpCreds,
    lane_dir: &Path,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<proc::RunOut, String> {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let eml_path = lane_dir.join(format!("_outreach_send_{epoch}_{}.eml", std::process::id()));
    let eml = format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n",
        from = c.from,
    );
    let _ = std::fs::create_dir_all(lane_dir);
    std::fs::write(&eml_path, eml.as_bytes()).map_err(|e| format!("eml write: {e}"))?;
    let _guard = TempEmlGuard(eml_path.clone());
    let argv = curl_smtp_argv(c, &eml_path.to_string_lossy(), to);
    proc::run(&argv, None, Some(Duration::from_secs(SEND_TIMEOUT_S)))
        .map_err(|e| format!("curl: {e}"))
}

// --------------------------------------------------------------------------- //
// shared small IO helpers
// --------------------------------------------------------------------------- //

/// Read a small JSON state file; absent/corrupt reads as `{}` (fail-open toward "fresh state" —
/// the caps and dedup then start clean, which only ever draws MORE human review, never less).
fn read_json_state(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
}

/// Atomic state write (temp + rename); best-effort dir create. Returns false on failure so a
/// caller that must be STAMP-FIRST can refuse to proceed without a durable claim.
fn write_json_state(path: &Path, st: &Value) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    proc::atomic_write_json(path, st).is_ok()
}

/// Append one record to an append-only audit .jsonl (OSError -> pass, the notify log contract).
fn append_jsonl(path: &Path, record: &Value) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{}", serde_json::to_string(record).unwrap_or_default())?;
        Ok(())
    })();
}

/// Page a needs card ONCE per (lane, which): the marker is claimed with `create_new` so exactly
/// one OS process (GUI tick vs Sentinel one-shot) pages even when both race the same sweep —
/// never a per-sweep page-flood (catalog #8).
fn page_needs_once(repo: &Value, which: &str, title: String, body: String) {
    let Some(p) = needs_marker_path(repo, which) else {
        return;
    };
    if p.exists() {
        return;
    }
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
        .is_ok()
    {
        let _ = notify::send(&Notice::red(title, body));
    }
}

/// Seed the operator-owned empty targets template (atomic). Solomon NEVER fills it in.
fn seed_targets_template(path: &Path) {
    let template = json!({
        "comment": "Operator-owned cold-outreach target list for this lane. Solomon NEVER scrapes \
                    or invents contacts. Add entries by hand; email + rationale are required (a \
                    target with no rationale is skipped).",
        "targets": [],
    });
    let _ = write_json_state(path, &template);
}

// --------------------------------------------------------------------------- //
// COMPOSE seam — maybe_draft_outreach (day-gated, honest-trigger, budget-aware)
// --------------------------------------------------------------------------- //

/// The every-sweep cold-outreach COMPOSER, ridden on `ceo_slow_tail`: at most ONE gated, unsent,
/// persona-safe draft per invocation, for the first eligible PUBLIC lane with (an operator-seeded
/// target left to contact) AND (today's planner outreach directive) AND (fleet budget headroom).
/// Mirrors `growth::maybe_draft_growth_content` — priority-ordered lanes, honest trigger,
/// stamp-first state, `<=1 lane per tail invocation`.
pub fn maybe_draft_outreach(snapshot: &Value, status: &Value) {
    super::seam_marker("outreach_compose");
    let today = local_today();
    let today_marker = super::ceo_marker(&today);
    // BUDGET PREFLIGHT (edge E4): when the fleet has burned `daily_call_budget`, compose nothing
    // this sweep — no LLM spend past the shared quota. Nothing is recorded; the counter resets by
    // date and the next in-budget sweep retries.
    let remaining = crate::fleet::daily_calls_remaining();

    // Priority-ordered lane inventory (the growth composer's exact ordering discipline).
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
        if !repo.get("public").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let fallback = repo.get("goal").and_then(Value::as_str).unwrap_or("");
        let goal_md = std::fs::read_to_string(super::goal_post_path(&lane)).ok();
        let north_star = super::pick_goal_post(goal_md.as_deref(), fallback);
        let rollup = status
            .pointer(&format!("/projects/{lane}"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !crate::ceo::growth::eligible_repo(&repo, &north_star, &rollup) {
            continue;
        }
        let consumed = draft_lane(
            &repo,
            &north_star,
            snapshot,
            &today,
            &today_marker,
            remaining,
            |user| {
                // Model + skill resolved at call time (never hardcoded) — the growth composer's exact
                // transport: the brain's creative worker when enabled, else the CEO model; `ctx: None`
                // routes via the CEO chat path.
                let model = crate::ceo::growth::pick_growth_model(
                    &crate::improver::brain::BrainConfig::from_autopilot(),
                );
                let skill = crate::improver::brain::load_skill(&lane, "outreach");
                crate::improver::brain::spawn_worker(
                    &model,
                    OUTREACH_COMPOSER_PROMPT,
                    &skill,
                    user,
                    None,
                )
                .map(|reply| (model, reply))
            },
        );
        if consumed {
            return; // at most ONE compose attempt per tail invocation
        }
    }
}

/// Decide-and-compose for ONE eligible lane, with the LLM SEAM INJECTED (`compose` returns
/// `(model, reply)`) so every rung is testable hermetically. Returns true iff this lane CONSUMED
/// the invocation's compose attempt (drafted, or a named post-stamp skip); false = nothing to do
/// here, the caller may try the next lane.
///
/// Control flow (spec §3.5): budget -> honest targets -> honest trigger -> day-cap + dedup ->
/// STAMP-FIRST state write -> compose -> parse -> persona gate -> dated provenance-tagged append.
pub(crate) fn draft_lane<F>(
    repo: &Value,
    north_star: &str,
    snapshot: &Value,
    today: &str,
    today_marker: &str,
    calls_remaining: i64,
    compose: F,
) -> bool
where
    F: FnOnce(&str) -> Result<(String, String), String>,
{
    let lane = paths::repo_name(repo);
    let (Some(tpath), Some(dlog), Some(dstate)) = (
        targets_path(repo),
        drafts_log_path(repo),
        drafted_state_path(repo),
    ) else {
        return false; // nameless lane — no runtime dir (fail-closed, edge E21)
    };
    if calls_remaining <= 0 {
        return false; // fleet budget exhausted — no LLM spend, retry next in-budget sweep (E4)
    }
    // TARGETS DISCOVERY (honest, edge E1): absent file => seed the empty template + page ONCE +
    // skip. Solomon never fabricates a contact. Present-but-empty => skip silently (already
    // seeded/paged). A READ ERROR on an existing file => skip the sweep (never touch a
    // half-written file — the growth backlog rule).
    if !tpath.exists() {
        seed_targets_template(&tpath);
        page_needs_once(
            repo,
            "targets",
            format!("Solomon: outreach needs targets -> {lane}"),
            format!(
                "Solomon can cold-email for {lane} but has no targets. Add operator-vetted \
                 contacts to runtime\\{lane}\\outreach_targets.json (email + rationale required). \
                 Solomon never scrapes or invents contacts."
            ),
        );
        return false;
    }
    let raw = match std::fs::read_to_string(&tpath) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let targets = load_targets(&raw);
    if targets.is_empty() {
        return false;
    }
    // HONEST TRIGGER (edge E6): today's planner outreach directive, or a quiet day. A backlog
    // read error (improver mid-rewrite) skips without stamping.
    let backlog = match std::fs::read_to_string(super::backlog_path(&lane)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let Some(directive) = outreach_directive(&backlog, today_marker) else {
        return false;
    };
    // DAY-CAP + DEDUP (edges E2/E10-compose): reset the day counter on a date roll; pick the
    // first never-drafted target under today's cap.
    let mut st = read_json_state(&dstate);
    if st.get("day").and_then(Value::as_str) != Some(today) {
        st["day"] = json!(today);
        st["day_count"] = json!(0);
    }
    let (max_per_day, _, _) = caps(repo);
    let day_count = st.get("day_count").and_then(Value::as_i64).unwrap_or(0);
    if day_count >= max_per_day {
        return false;
    }
    let drafted: Vec<String> = st
        .get("drafted_target_keys")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let Some(target) = targets.into_iter().find(|t| {
        let email = t.get("email").and_then(Value::as_str).unwrap_or("");
        !drafted.contains(&target_key(email))
    }) else {
        return false; // every seeded target already drafted — never a redraft (E2)
    };
    let key = target_key(target.get("email").and_then(Value::as_str).unwrap_or(""));

    // STAMP-FIRST (the growth claim discipline): persist the claimed target + bumped day count
    // BEFORE the slow LLM call, so a hung/failed compose consumes the attempt and a concurrent
    // process re-reading the state does not double-fire on the same target. If the claim cannot
    // be made durable, do NOT compose (fail-closed).
    let mut keys = drafted.clone();
    keys.push(key.clone());
    st["drafted_target_keys"] = json!(keys);
    st["day_count"] = json!(day_count + 1);
    st["last"] = json!("attempt");
    if !write_json_state(&dstate, &st) {
        return false;
    }
    let record_last = |st: &mut Value, note: &str| {
        st["last"] = json!(note);
        let _ = write_json_state(&dstate, st);
    };

    // COMPOSE (<=1 target per call): the evidence substrate is the snapshot this tail already
    // holds — outcomes_24h + velocity; the draft cites real numbers, never invented traction.
    let outcomes = snapshot
        .pointer(&format!("/projects/{lane}"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let velocity = super::velocity_context(&outcomes, north_star);
    let user = serde_json::to_string_pretty(&json!({
        "lane": lane,
        "brand": lane,
        "north_star": north_star,
        "directive": directive,
        "target": {
            "name": target.get("name").cloned().unwrap_or(json!("")),
            "org": target.get("org").cloned().unwrap_or(json!("")),
            "rationale": target.get("rationale").cloned().unwrap_or(json!("")),
        },
        "outcomes_24h": outcomes,
        "velocity": velocity,
    }))
    .unwrap_or_default();

    let (model, reply) = match compose(&user) {
        Ok(x) => x,
        Err(e) => {
            // LLM unavailable (quota/429/parked): named skip, attempt stays consumed — never a
            // fabricated draft (edge E5, the growth precedent).
            record_last(
                &mut st,
                &format!("skip:llm_unavailable {}", super::cap_line(&e, 160)),
            );
            return true;
        }
    };
    let Some((subject, body)) = parse_outreach_reply(&reply) else {
        record_last(&mut st, "skip:unparseable_or_empty");
        return true;
    };
    // The deterministic PERSONA gate (edge E7) — the prompt rule is a wish, this is the gate.
    if violates_persona(&format!("{subject} {body}")) {
        record_last(&mut st, "skip:persona_violation");
        return true;
    }
    let prompt_hash = sha8(&user);
    let epoch_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fact = draft_fact(
        &model,
        &prompt_hash,
        &lane,
        &target,
        &subject,
        &body,
        epoch_s,
    );
    match ObservationLog::at(dlog).append_fact(today, &fact) {
        Ok(()) => {
            record_last(&mut st, "ok");
            // AUTONOMOUS SEND QUEUE: persist the FULL sendable payload to the outbox so the auto-send
            // seam has the complete message (the drafts log stores only a truncated body head). The
            // recipient is the OPERATOR-SUPPLIED target's own email — Solomon never invents a contact.
            let to = target
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if let Some(obx) = outbox_path(repo) {
                append_jsonl(
                    &obx,
                    &json!({
                        "ts": today,
                        "target_key": key,
                        "to": to,
                        "subject": subject,
                        "body": body,
                        "signature": send_signature(&to, &subject, &body),
                    }),
                );
            }
            let _ = notify::send(&Notice::report(
                format!("Solomon: outreach draft -> {lane}"),
                format!(
                    "{subject} (queued for autonomous send behind rate/persona/target guards — \
                     runtime\\{lane}\\outreach_outbox.jsonl)"
                ),
            ));
        }
        Err(_) => record_last(&mut st, "skip:append_rejected"),
    }
    true
}

// --------------------------------------------------------------------------- //
// SEND seam — maybe_auto_send_outreach (every sweep, AUTONOMOUS behind automated guards)
// --------------------------------------------------------------------------- //

/// The every-sweep AUTO-SEND seam, ridden on `ceo_slow_tail`. Sends the oldest UNSENT outbox entry
/// AUTONOMOUSLY — NO operator `approved:true` wait — behind AUTOMATED guardrails only (operator-
/// target guard, rate caps, persona, idempotency, honest footer). SMTP-absent stays fully inert (a
/// DATA dependency, not a human gate): with no creds the seam pages the needs card ONCE, sends none.
pub fn maybe_auto_send_outreach(_snapshot: &Value, _status: &Value) {
    super::seam_marker("outreach_send");
    let creds = smtp_creds();
    for repo in crate::control::registry::read_repos_json() {
        if !repo.get("public").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let _ = send_lane(&repo, creds.as_ref(), send_via_curl);
    }
}

/// Decide-and-send for ONE lane with the SENDER SEAM INJECTED (testable without curl/SMTP). The
/// AUTOMATED-only ladder that REPLACES the human approval gate: pick the oldest UNSENT outbox entry
/// -> well-formed {to,subject,body} -> the recipient is on the OPERATOR-SUPPLIED target list (never
/// a fabricated/scraped contact) -> not already sent (signature idempotency) -> SMTP creds present
/// (else needs card ONCE, stay inert) -> under the daily/hourly rate caps -> persona + truthful-
/// content re-check -> honest sender-identity/opt-out footer -> send. The signature is recorded ONLY
/// on exit 0, so a transient failure stays retriable (edge E9) and a success can never re-send (E10).
pub(crate) fn send_lane<F>(repo: &Value, creds: Option<&SmtpCreds>, sender: F) -> Value
where
    F: FnOnce(&SmtpCreds, &Path, &str, &str, &str) -> Result<proc::RunOut, String>,
{
    let lane = paths::repo_name(repo);
    let skip = |reason: &str| json!({"lane": lane, "sent": false, "reason": reason});
    let (Some(obx), Some(tpath), Some(sstate), Some(slog), Some(dir)) = (
        outbox_path(repo),
        targets_path(repo),
        sent_state_path(repo),
        sent_log_path(repo),
        paths::runtime_dir(repo),
    ) else {
        return json!({"lane": lane, "sent": false, "reason": "nameless lane"});
    };
    // Already-sent signatures — needed to pick the oldest UNSENT outbox entry AND to dedup below.
    let mut st = read_json_state(&sstate);
    let sent_sigs: Vec<String> = st
        .get("sent_signatures")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Oldest UNSENT outbox entry (FIFO — nothing starves; the rate cap paces the queue).
    let Some(entry) = read_outbox(&obx)
        .into_iter()
        .find(|e| !sent_sigs.contains(&outbox_signature(e)))
    else {
        return skip("nothing to send");
    };
    // The idempotency key of the chosen entry (the scan guarantees it is not yet sent).
    let signature = outbox_signature(&entry);
    let to = entry
        .get("to")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let subject = super::cap_line(
        entry.get("subject").and_then(Value::as_str).unwrap_or(""),
        200,
    );
    let body = entry
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // WELL-FORMED (edge E10): non-empty to/subject/body + a whitespace-free @-carrying recipient
    // (headers are single-line-flattened downstream, so no header injection).
    if to.is_empty()
        || subject.is_empty()
        || body.is_empty()
        || !to.contains('@')
        || to.chars().any(char::is_whitespace)
    {
        return skip("malformed outbox entry");
    }
    // OPERATOR-TARGET GUARD (the hard anti-scrape line): the recipient MUST be on the lane's
    // operator-supplied target list. Solomon never fabricates, guesses, or scrapes a contact — a
    // recipient not on the operator's list is refused even if it somehow reached the outbox.
    let targets_raw = std::fs::read_to_string(&tpath).unwrap_or_default();
    let target_keys: Vec<String> = load_targets(&targets_raw)
        .iter()
        .filter_map(|t| t.get("email").and_then(Value::as_str))
        .map(target_key)
        .collect();
    if !target_keys.contains(&target_key(&to)) {
        return skip("recipient not in operator targets");
    }
    // SMTP PRESENCE (edge E8): queued but no creds => page the needs card ONCE, stay inert. This is
    // a DATA dependency, not a human approval — the ONLY thing gating an otherwise-autonomous send.
    let Some(c) = creds else {
        page_needs_once(
            repo,
            "smtp",
            format!("Solomon: outreach queued but no SMTP -> {lane}"),
            format!(
                "An outreach message for {lane} is queued to send but no SMTP creds are set. Add \
                 SOLOMON_SMTP_HOST/USER/PASS (+optional PORT/FROM) to <HERE>\\.env. It stays unsent."
            ),
        );
        return skip("no smtp creds");
    };
    // RATE CAPS (edge E11): retry at the next eligible window, never a burst.
    let (_, daily_cap, hourly_cap) = caps(repo);
    if !rate_ok(&st, daily_cap, hourly_cap) {
        return skip("rate capped");
    }
    // PERSONA + truthful-content re-check (edge E7) — belt-and-suspenders over the compose-time gate.
    if violates_persona(&format!("{subject} {body}")) {
        let _ = notify::send(&Notice::red(
            format!("Solomon: outreach send REFUSED -> {lane}"),
            "the queued message carries an operator-identifying marker (persona rule) — skipped"
                .to_string(),
        ));
        return skip("persona violation");
    }
    // HONEST FOOTER: identify the sender + offer an opt-out before the message leaves (truthful-
    // content safety for autonomous sending). The signature above is over the PRE-footer body so it
    // stays stable against the deterministic footer.
    let send_body = ensure_footer(&body, &lane);
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    match sender(c, &dir, &to, &subject, &send_body) {
        Ok(out) if out.code == 0 => {
            bump_rate(&mut st);
            let mut sigs: Vec<String> = st
                .get("sent_signatures")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            sigs.push(signature.clone());
            if sigs.len() > 500 {
                let cut = sigs.len() - 500; // bound the state file; oldest signatures age out
                sigs.drain(..cut);
            }
            st["sent_signatures"] = json!(sigs);
            let _ = write_json_state(&sstate, &st);
            append_jsonl(
                &slog,
                &json!({"ts": ts, "to": to, "subject": subject, "message_id": "",
                        "signature": signature, "result": "sent", "detail": ""}),
            );
            let _ = notify::send(&Notice::report(
                format!("Solomon: outreach SENT -> {lane}"),
                format!("to {to} subj {subject}"),
            ));
            json!({"lane": lane, "sent": true})
        }
        Ok(out) => {
            let detail = format!(
                "curl exit {}: {}",
                out.code,
                super::cap_line(&out.stderr, 200)
            );
            append_jsonl(
                &slog,
                &json!({"ts": ts, "to": to, "subject": subject, "message_id": "",
                        "signature": signature, "result": "failed", "detail": detail}),
            );
            let _ = notify::send(&Notice::red(
                format!("Solomon: outreach send FAILED -> {lane}"),
                detail.clone(),
            ));
            json!({"lane": lane, "sent": false, "reason": detail})
        }
        Err(e) => {
            // Spawn error / timeout (incl. a curl-less host, edge E22): audited + paged, never a
            // false "sent"; the signature is NOT recorded, so a later sweep can retry.
            append_jsonl(
                &slog,
                &json!({"ts": ts, "to": to, "subject": subject, "message_id": "",
                        "signature": signature, "result": "failed", "detail": e}),
            );
            let _ = notify::send(&Notice::red(
                format!("Solomon: outreach send FAILED -> {lane}"),
                e.clone(),
            ));
            json!({"lane": lane, "sent": false, "reason": e})
        }
    }
}

// --------------------------------------------------------------------------- //
// tests — the outreach acceptance contracts (hermetic: temp HERE + unique lanes)
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    /// A PUBLIC repo row with a UNIQUE lane name (parallel-test isolation by unique paths).
    fn uniq_repo(tag: &str) -> Value {
        let name = format!(
            "outreach_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        json!({ "name": name, "public": true })
    }

    fn lane_of(repo: &Value) -> String {
        paths::repo_name(repo)
    }

    fn seed_targets(repo: &Value, targets: Value) {
        let p = targets_path(repo).unwrap();
        assert!(write_json_state(&p, &json!({"targets": targets})));
    }

    fn seed_backlog(lane: &str, line: &str) {
        let p = super::super::backlog_path(lane);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("- [ ] {line}\n")).unwrap();
    }

    fn cleanup(repo: &Value) {
        if let Some(dir) = paths::runtime_dir(repo) {
            let _ = std::fs::remove_dir_all(dir);
        }
        let lane = lane_of(repo);
        let _ = std::fs::remove_dir_all(paths::here().join("improver").join(lane));
    }

    fn today() -> String {
        local_today()
    }

    fn marker() -> String {
        super::super::ceo_marker(&today())
    }

    fn ok_reply() -> Result<(String, String), String> {
        Ok((
            "model-x".to_string(),
            "{\"subject\":\"Quick intro from the project\",\"body\":\"We ship a small tool; your \
             community writes about exactly this. 2 features shipped this week.\"}"
                .to_string(),
        ))
    }

    fn one_target() -> Value {
        json!([{ "email": "Person@Org.com", "name": "Person", "org": "Org",
                 "rationale": "writes about this exact niche" }])
    }

    // -------- 1: the honest trigger (pure) --------
    #[test]
    fn outreach_directive_matches_today_or_campaign_outreach_lines_only() {
        let m = "(ceo 2026-07-16)";
        let backlog = "\
- [x] [chore] done thing (ceo 2026-07-16)\n\
- [ ] [feature] improve README quickstart (ceo 2026-07-16)\n\
- [ ] [feature] run cold outreach to 3 newsletter authors (ceo 2026-07-16)\n\
- [ ] [feature] reach out to partners (deferred 2026-07-15) (ceo 2026-07-16)\n";
        let d = outreach_directive(backlog, m).expect("the outreach line matches");
        assert!(d.contains("cold outreach to 3 newsletter authors"), "{d}");
        // a growth-only line is NOT an outreach directive; stale-day lines don't match either
        assert!(
            outreach_directive("- [ ] [feature] improve README (ceo 2026-07-16)\n", m).is_none()
        );
        assert!(
            outreach_directive(
                "- [ ] [feature] cold outreach to partners (ceo 2026-07-15)\n",
                m
            )
            .is_none()
        );
        // campaign steps qualify without today's marker
        assert!(
            outreach_directive("- [ ] [campaign:q3] pitch email to maintainers\n", m).is_some()
        );
        // keyword set positives + negatives
        for pos in [
            "cold email",
            "email campaign",
            "reach out",
            "contact list",
            "pitch email",
        ] {
            assert!(is_outreach_directive(pos), "{pos}");
        }
        for neg in ["fix the trader bug", "improve README", "ship release notes"] {
            assert!(!is_outreach_directive(neg), "{neg}");
        }
    }

    // -------- 2 + 3: honest targets + stable keys (pure) --------
    #[test]
    fn load_targets_drops_entries_missing_email_or_rationale() {
        let raw = r#"{"comment": "x", "targets": [
            {"email": "a@b.c", "rationale": "fits"},
            {"email": "no-rationale@b.c"},
            {"rationale": "no email"},
            {"email": "  ", "rationale": "blank email"},
            {"email": "b@c.d", "rationale": "  "}
        ]}"#;
        let t = load_targets(raw);
        assert_eq!(t.len(), 1, "only the complete entry survives: {t:?}");
        assert_eq!(t[0]["email"], "a@b.c");
        // malformed / empty bodies degrade to no targets, never a panic
        assert!(load_targets("not json").is_empty());
        assert!(load_targets("{}").is_empty());
    }

    #[test]
    fn target_key_is_stable_and_normalizes_case_and_whitespace() {
        let k = target_key("Person@Org.com");
        assert_eq!(k.len(), 8);
        assert_eq!(
            k,
            target_key("  person@org.com  "),
            "trim + lowercase normalize"
        );
        assert_ne!(k, target_key("other@org.com"));
    }

    // -------- 4: the draft fact carries full provenance (pure) --------
    #[test]
    fn draft_fact_shape_carries_gate_tag_target_and_provenance() {
        let t = json!({"email": "p@o.com", "rationale": "writes about this"});
        let fact = draft_fact(
            "m1",
            "ff00aa11",
            "sover",
            &t,
            "Subject line",
            "Body text",
            1752,
        );
        assert!(
            fact.starts_with("rsi: outreach DRAFT [GATED, unsent, cold-email]"),
            "{fact}"
        );
        assert!(fact.contains("lane=sover"), "{fact}");
        assert!(
            fact.contains(&format!("target={}", target_key("p@o.com"))),
            "{fact}"
        );
        assert!(fact.contains("to=p@o.com"), "{fact}");
        assert!(fact.contains("subj=Subject line"), "{fact}");
        assert!(
            fact.contains("(t=1752 model=m1 prompt=ff00aa11 rationale=writes about this)"),
            "{fact}"
        );
        // it passes the observation-log fact gate (dated datum present, no summary shape)
        assert!(ObservationLog::validate_fact(&fact).is_fact());
    }

    // -------- 5: reply parsing (pure) --------
    #[test]
    fn parse_outreach_reply_extracts_subject_and_body_or_none() {
        let fenced = "Sure!\n```json\n{\"subject\": \"Hi\", \"body\": \"We built a thing.\"}\n```";
        let (s, b) = parse_outreach_reply(fenced).expect("fenced JSON parses");
        assert_eq!(s, "Hi");
        assert_eq!(b, "We built a thing.");
        // empty body / empty subject / no JSON => None (fail-closed, nothing drafted)
        assert!(parse_outreach_reply("{\"subject\": \"Hi\", \"body\": \"\"}").is_none());
        assert!(parse_outreach_reply("{\"subject\": \"\", \"body\": \"x\"}").is_none());
        assert!(parse_outreach_reply("no json here").is_none());
    }

    // -------- 6: compose writes a GATED unsent draft to disk --------
    #[test]
    fn compose_writes_a_gated_unsent_provenance_tagged_draft() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = uniq_repo("compose");
        let lane = lane_of(&repo);
        seed_targets(&repo, one_target());
        seed_backlog(
            &lane,
            &format!(
                "[feature] run cold outreach to the vetted list {}",
                marker()
            ),
        );

        let consumed = draft_lane(
            &repo,
            "grow the project",
            &json!({}),
            &today(),
            &marker(),
            10,
            |user| {
                assert!(
                    user.contains("north_star"),
                    "the compose payload is the evidence JSON: {user}"
                );
                assert!(
                    user.contains("writes about this exact niche"),
                    "rationale rides along: {user}"
                );
                ok_reply()
            },
        );
        assert!(
            consumed,
            "an eligible lane with a directive + target composes"
        );

        let body =
            std::fs::read_to_string(drafts_log_path(&repo).unwrap()).expect("drafts log written");
        let line = body.lines().next().unwrap();
        assert!(line.contains('\t'), "dated line: {line}");
        assert!(
            line.contains("rsi: outreach DRAFT [GATED, unsent, cold-email]"),
            "{line}"
        );
        assert!(line.contains("to=Person@Org.com"), "{line}");
        assert!(line.contains("model=model-x"), "{line}");

        // the AUTONOMOUS send queue carries the FULL sendable payload (not a truncated body head)
        let obx = read_outbox(&outbox_path(&repo).unwrap());
        assert_eq!(
            obx.len(),
            1,
            "compose queues exactly one outbox entry: {obx:?}"
        );
        assert_eq!(obx[0]["to"], "Person@Org.com");
        assert!(
            obx[0]["body"]
                .as_str()
                .unwrap()
                .contains("2 features shipped this week"),
            "the outbox holds the FULL body: {}",
            obx[0]["body"]
        );
        assert_eq!(
            obx[0]["signature"],
            send_signature(
                "Person@Org.com",
                "Quick intro from the project",
                "We ship a small tool; your community writes about exactly this. 2 features shipped this week."
            )
        );

        let st = read_json_state(&drafted_state_path(&repo).unwrap());
        assert_eq!(st["last"], "ok");
        assert_eq!(st["day_count"], 1);
        assert_eq!(st["drafted_target_keys"][0], target_key("person@org.com"));

        cleanup(&repo);
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // -------- 7: dedup — a drafted target is never redrafted --------
    #[test]
    fn a_target_already_drafted_is_never_redrafted() {
        let repo = uniq_repo("dedup");
        let lane = lane_of(&repo);
        seed_targets(&repo, one_target());
        seed_backlog(
            &lane,
            &format!("[feature] cold outreach continues {}", marker()),
        );
        // pre-seed the state as if this target was drafted on an earlier sweep today
        let st = json!({"day": today(), "day_count": 1,
                        "drafted_target_keys": [target_key("person@org.com")]});
        assert!(write_json_state(&drafted_state_path(&repo).unwrap(), &st));

        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 10, |_| {
            panic!("the composer must NOT be called for an already-drafted target")
        });
        assert!(!consumed, "nothing left to draft — the lane is skipped");
        cleanup(&repo);
    }

    // -------- 8: the day cap blocks further drafts that day --------
    #[test]
    fn day_cap_blocks_composing_past_max_drafts_per_day() {
        let mut repo = uniq_repo("daycap");
        repo["outreach"] = json!({"max_drafts_per_day": 2});
        let lane = lane_of(&repo);
        seed_targets(
            &repo,
            json!([
                {"email": "fresh@x.com", "rationale": "fits"},
            ]),
        );
        seed_backlog(
            &lane,
            &format!("[feature] cold outreach batch {}", marker()),
        );
        let st = json!({"day": today(), "day_count": 2, "drafted_target_keys": ["deadbeef"]});
        assert!(write_json_state(&drafted_state_path(&repo).unwrap(), &st));

        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 10, |_| {
            panic!("the composer must NOT be called past the day cap")
        });
        assert!(!consumed);
        // a NEW day resets the counter: the same state with a stale day composes again
        let st = json!({"day": "1999-01-01", "day_count": 2, "drafted_target_keys": ["deadbeef"]});
        assert!(write_json_state(&drafted_state_path(&repo).unwrap(), &st));
        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 10, |_| {
            ok_reply()
        });
        assert!(consumed, "the date roll resets the day cap");
        cleanup(&repo);
    }

    // -------- 9: absent targets => seed template + needs card ONCE --------
    #[test]
    fn absent_targets_seeds_template_and_pages_needs_card_once() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = uniq_repo("needs");
        let lane = lane_of(&repo);
        seed_backlog(&lane, &format!("[feature] cold outreach {}", marker()));

        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 10, |_| {
            panic!("no targets — the composer must not fire")
        });
        assert!(!consumed);
        // the empty operator template was seeded and the needs marker dropped
        let tpl = read_json_state(&targets_path(&repo).unwrap());
        assert_eq!(tpl["targets"], json!([]));
        assert!(tpl["comment"].as_str().unwrap().contains("NEVER scrapes"));
        let marker_path = needs_marker_path(&repo, "targets").unwrap();
        assert!(
            marker_path.exists(),
            "the needs card marker dedupes future pages"
        );
        let mtime = std::fs::metadata(&marker_path).unwrap().modified().unwrap();

        // a second sweep with the (still empty) template skips SILENTLY: no re-page, marker untouched
        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 10, |_| {
            panic!("still no targets")
        });
        assert!(!consumed);
        assert_eq!(
            std::fs::metadata(&marker_path).unwrap().modified().unwrap(),
            mtime,
            "the needs marker is claimed once, never rewritten per sweep"
        );
        cleanup(&repo);
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // -------- 10: SEND refuses a recipient NOT on the operator target list (hard anti-scrape) -----
    #[test]
    fn send_refuses_a_recipient_not_on_the_operator_target_list() {
        let repo = uniq_repo("sendgate");
        // the operator seeded ONE target; the outbox somehow holds a DIFFERENT recipient
        seed_targets(
            &repo,
            json!([{"email": "listed@org.com", "rationale": "fits"}]),
        );
        seed_outbox(&repo, "not-listed@elsewhere.com", "s", "an honest body");

        let creds = SmtpCreds {
            host: "h".into(),
            port: 465,
            user: "u".into(),
            pass: "p".into(),
            from: "u".into(),
        };
        let out = send_lane(&repo, Some(&creds), |_, _, _, _, _| {
            panic!("a recipient off the operator target list must NEVER send")
        });
        assert_eq!(out["sent"], false);
        assert_eq!(out["reason"], "recipient not in operator targets");

        // an EMPTY outbox is simply inert (nothing to send), never a send
        let repo2 = uniq_repo("sendempty");
        seed_targets(&repo2, one_target());
        let out2 = send_lane(&repo2, Some(&creds), |_, _, _, _, _| {
            panic!("empty outbox never sends")
        });
        assert_eq!(out2["reason"], "nothing to send");
        cleanup(&repo);
        cleanup(&repo2);
    }

    // -------- 11: queued + SMTP absent => needs card once, no send, no signature (DATA dependency) --
    #[test]
    fn queued_without_smtp_pages_needs_card_and_stays_unsent() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = uniq_repo("nosmtp");
        seed_targets(&repo, json!([{"email": "p@o.com", "rationale": "fits"}]));
        seed_outbox(
            &repo,
            "p@o.com",
            "Hello",
            "We built a thing worth your time.",
        );

        let out = send_lane(&repo, None, |_, _, _, _, _| {
            panic!("no creds — the sender must not fire")
        });
        assert_eq!(out["sent"], false);
        assert_eq!(out["reason"], "no smtp creds");
        assert!(
            needs_marker_path(&repo, "smtp").unwrap().exists(),
            "needs card paged (deduped)"
        );
        let st = read_json_state(&sent_state_path(&repo).unwrap());
        assert!(
            st.get("sent_signatures").is_none(),
            "no signature recorded — retriable once creds exist"
        );
        cleanup(&repo);
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // -------- 12: AUTO-send (no approval) => exactly one send with the honest footer, idempotent ---
    #[test]
    fn auto_send_with_smtp_sends_exactly_once_with_footer() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = uniq_repo("sendonce");
        let lane = lane_of(&repo);
        seed_targets(&repo, json!([{"email": "p@o.com", "rationale": "fits"}]));
        seed_outbox(&repo, "p@o.com", "Hello", "Honest body.");
        let creds = SmtpCreds {
            host: "h".into(),
            port: 465,
            user: "u".into(),
            pass: "p".into(),
            from: "u".into(),
        };

        let mut calls = 0;
        let out = send_lane(&repo, Some(&creds), |_, _, to, subject, body| {
            calls += 1;
            assert_eq!(to, "p@o.com");
            assert_eq!(subject, "Hello");
            // the composed body rides AND the honest sender-identity/opt-out footer is attached
            assert!(
                body.contains("Honest body."),
                "the composed body rides: {body}"
            );
            assert!(
                body.contains(&format!("the {lane} project's automated outreach")),
                "footer id: {body}"
            );
            assert!(
                body.to_lowercase().contains("reply"),
                "opt-out line present: {body}"
            );
            Ok(proc::RunOut {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        });
        assert_eq!(
            out["sent"], true,
            "an auto-send fires with NO operator approval: {out}"
        );
        assert_eq!(calls, 1);
        // the audit trail + signature landed
        let audit = std::fs::read_to_string(sent_log_path(&repo).unwrap()).unwrap();
        assert!(audit.contains("\"result\":\"sent\""), "{audit}");
        let st = read_json_state(&sent_state_path(&repo).unwrap());
        assert_eq!(st["sent_signatures"].as_array().unwrap().len(), 1);
        assert_eq!(st["count"], 1);

        // the SECOND sweep does NOT re-send the same message (signature idempotency)
        let out = send_lane(&repo, Some(&creds), |_, _, _, _, _| {
            panic!("an already-sent signature must never re-send")
        });
        assert_eq!(out["sent"], false);
        assert_eq!(out["reason"], "nothing to send");
        cleanup(&repo);
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // -------- 12b: a FAILED send records no signature (retriable) --------
    #[test]
    fn failed_send_is_audited_and_retriable() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = uniq_repo("sendfail");
        seed_targets(&repo, json!([{"email": "p@o.com", "rationale": "fits"}]));
        seed_outbox(&repo, "p@o.com", "Hello", "Body.");
        let creds = SmtpCreds {
            host: "h".into(),
            port: 465,
            user: "u".into(),
            pass: "p".into(),
            from: "u".into(),
        };

        let out = send_lane(&repo, Some(&creds), |_, _, _, _, _| {
            Ok(proc::RunOut {
                code: 67,
                stdout: String::new(),
                stderr: "auth failed".into(),
            })
        });
        assert_eq!(out["sent"], false);
        let audit = std::fs::read_to_string(sent_log_path(&repo).unwrap()).unwrap();
        assert!(audit.contains("\"result\":\"failed\""), "{audit}");
        assert!(audit.contains("auth failed"), "{audit}");
        let st = read_json_state(&sent_state_path(&repo).unwrap());
        assert!(
            st.get("sent_signatures").is_none(),
            "failure records NO signature — retriable (E9)"
        );
        // the retry sweep CAN fire the sender again
        let out = send_lane(&repo, Some(&creds), |_, _, _, _, _| {
            Ok(proc::RunOut {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        });
        assert_eq!(
            out["sent"], true,
            "a transient failure stays retriable: {out}"
        );
        cleanup(&repo);
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // -------- 13: rate caps (pure clock-injected port of sover email_outreach) --------
    #[test]
    fn rate_caps_block_daily_and_hourly_and_reset_by_date() {
        let mut st = json!({});
        // fresh state: allowed; bump twice in hour 10
        assert!(rate_ok_at(&st, 5, 1, "2026-07-16", 10));
        bump_rate_at(&mut st, "2026-07-16", 10);
        assert_eq!(st["count"], 1);
        // hourly cap (1/hour) blocks a second send the same hour
        assert!(!rate_ok_at(&st, 5, 1, "2026-07-16", 10));
        // the next hour is allowed again
        assert!(rate_ok_at(&st, 5, 1, "2026-07-16", 11));
        bump_rate_at(&mut st, "2026-07-16", 11);
        // daily cap blocks at N regardless of hour
        st["count"] = json!(5);
        assert!(!rate_ok_at(&st, 5, 1, "2026-07-16", 12));
        // a new date resets both
        assert!(rate_ok_at(&st, 5, 1, "2026-07-17", 12));
        bump_rate_at(&mut st, "2026-07-17", 12);
        assert_eq!(st["count"], 1, "date roll resets the counter");
        assert_eq!(st["date"], "2026-07-17");
    }

    // -------- 14: the curl SMTP argv (pure, pinned) --------
    #[test]
    fn curl_smtp_argv_shape_matches_port_and_carries_creds() {
        let c465 = SmtpCreds {
            host: "mail.example.com".into(),
            port: 465,
            user: "u@example.com".into(),
            pass: "pw".into(),
            from: "from@example.com".into(),
        };
        let argv = curl_smtp_argv(&c465, "C:\\x\\m.eml", "to@dest.com");
        assert_eq!(argv[0], "curl");
        assert!(argv.contains(&"--ssl-reqd".to_string()));
        assert!(
            argv.contains(&"smtps://mail.example.com:465".to_string()),
            "465 => smtps: {argv:?}"
        );
        assert!(argv.contains(&"--mail-from".to_string()));
        assert!(argv.contains(&"from@example.com".to_string()));
        assert!(argv.contains(&"--mail-rcpt".to_string()));
        assert!(argv.contains(&"to@dest.com".to_string()));
        assert!(argv.contains(&"--upload-file".to_string()));
        assert!(
            argv.contains(&"u@example.com:pw".to_string()),
            "--user carries creds: {argv:?}"
        );
        // bounded: -m 30 rides along
        let m = argv.iter().position(|a| a == "-m").unwrap();
        assert_eq!(argv[m + 1], "30");
        // 587 selects the STARTTLS smtp:// form (still --ssl-reqd)
        let c587 = SmtpCreds { port: 587, ..c465 };
        let argv = curl_smtp_argv(&c587, "m.eml", "to@dest.com");
        assert!(
            argv.contains(&"smtp://mail.example.com:587".to_string()),
            "587 => smtp: {argv:?}"
        );
        assert!(argv.contains(&"--ssl-reqd".to_string()));
    }

    // -------- 15: the persona gate refuses an operator-marked draft --------
    #[test]
    fn persona_gate_refuses_an_operator_marked_reply() {
        let repo = uniq_repo("persona");
        let lane = lane_of(&repo);
        seed_targets(&repo, one_target());
        seed_backlog(&lane, &format!("[feature] cold outreach {}", marker()));

        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 10, |_| {
            Ok((
                "m".to_string(),
                "{\"subject\":\"Note from Cayleb\",\"body\":\"hi there\"}".to_string(),
            ))
        });
        assert!(consumed, "the attempt is consumed (no retry storm)");
        assert!(
            !drafts_log_path(&repo).unwrap().exists(),
            "NO draft lands for a persona violation"
        );
        let st = read_json_state(&drafted_state_path(&repo).unwrap());
        assert_eq!(st["last"], "skip:persona_violation");
        cleanup(&repo);
    }

    // -------- 16: the budget preflight blocks composing at zero remaining --------
    #[test]
    fn budget_preflight_makes_no_llm_call_when_exhausted() {
        let repo = uniq_repo("budget");
        let lane = lane_of(&repo);
        seed_targets(&repo, one_target());
        seed_backlog(&lane, &format!("[feature] cold outreach {}", marker()));

        let consumed = draft_lane(&repo, "grow", &json!({}), &today(), &marker(), 0, |_| {
            panic!("budget exhausted — the composer must not fire")
        });
        assert!(!consumed);
        // nothing recorded: the sweep retries once the budget resets by date (edge E4)
        assert!(!drafted_state_path(&repo).unwrap().exists());
        cleanup(&repo);
    }

    // -------- helper: queue a FULL sendable payload in the outbox (the composer's product) --------
    fn seed_outbox(repo: &Value, to: &str, subject: &str, body: &str) {
        let p = outbox_path(repo).unwrap();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .unwrap();
        writeln!(
            f,
            "{}",
            json!({"ts": today(), "target_key": target_key(to), "to": to,
                   "subject": subject, "body": body,
                   "signature": send_signature(to, subject, body)})
        )
        .unwrap();
    }
}
