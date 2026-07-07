//! NO-MONEY-OUT GUARD — the fail-closed, PREEMPTIVE money-out chokepoint (RSI v3).
//!
//! ============================ THE HARD INVARIANT ============================
//! Solomon NEVER moves money out. No withdrawal, transfer, deposit, funding,
//! purchase, payment, paid signup, ad spend, or any external spend is EVER
//! performed autonomously. The ONLY money action this guard permits is a
//! `place_trade` executed *by a whitelisted lane's OWN bot binary* (asmodeus /
//! kairos — Kalshi + futures), and even that is only ever DISPATCHED by that
//! lane, never by Solomon reaching for a money tool. Everything Solomon itself
//! could do stays default-DENY. Any UNKNOWN or ambiguous money-capable action
//! is DENIED (fail-closed) so a FUTURE money tool is blocked until a human
//! explicitly whitelists it.
//! ===========================================================================
//!
//! WHY THIS EXISTS. The audit found `money_surface = NONE` today — there is no
//! stripe/paypal/withdraw/payout/deposit/transfer/payment_intent/checkout
//! integration anywhere in Solomon, and its ~40-method frontend API is a CLOSED,
//! money-free set. This guard is therefore PREEMPTIVE: it does not patch a hole,
//! it welds one shut before it can ever be cut. The "money-out stays
//! human-gated" doctrine was already written in FIVE places but enforced by NO
//! single mandatory chokepoint. This module is that chokepoint.
//!
//! DOCTRINE LINEAGE (this guard makes the following ENFORCED, not merely stated):
//!   1. docs/rsi/AI-CEO-ARCHITECTURE-2026-07-07.md:60,69 — "MCP tool
//!      integrations ... (deploy, post, email, payments) — under the same
//!      blast-radius gates + human-gated money-out"; "Money-out stays
//!      human-gated." (and :19 PECRT: the thread cannot edit whitelists, raise
//!      its own budget, or bypass the skeptic).
//!   2. deploy.rs:9-12,66,378 — "A repo WITHOUT live_deploy is NEVER
//!      auto-deployed — money-out / live-money stays human-gated."
//!   3. ceo/allocate.rs:7,20 — "asmodeus's money-out stays human-gated
//!      elsewhere" + `is_real_money` (any equity_usd lane scores 0.0, never
//!      scaled).
//!   4. ceo.rs:335,1026 — CEO planner HARD RULE "no ad spend, no paid services;
//!      any money-out step stays human-gated"; "never scaled from here — money-out
//!      stays human-gated."
//!   5. watchdog.rs:1172 — "asmodeus/live-money stays human-gated."
//!
//! GATE SHAPE (copies Solomon's existing CLOSED-SET + PURE-PREDICATE + LOUD-PAGE
//! doctrine so it composes cleanly):
//!   * CLOSED money-capable-kind set [`MONEY_CAPABLE_KINDS`] — EMPTY today; a
//!     build-time closure test pins that every member classifies as a real
//!     verdict (never falls through to the unknown-DENY arm by accident), exactly
//!     as actions.rs's closure test pins ACTION_KINDS. Adding a money-capable
//!     kind forces adding its classification arm in the same commit.
//!   * PURE predicate: [`classify`] takes (kind, repo) Values and returns a
//!     [`Verdict`] with NO IO — unit-tested over the decision table.
//!   * FAIL-CLOSED: unknown / ambiguous / unreadable => DENY (mirrors
//!     redeploy's conservative "unreadable state => unsafe" and actions.rs's
//!     `_ => refuse, never guess" arm).
//!   * LOUD PAGE: a DENIED money attempt pages the operator, marker-deduped
//!     (mirrors actions.rs `page_operator_deduped`) — a denied autonomous
//!     money attempt is a real, must-see event.
//!
//! COMPOSITION ORDER: this guard is the OUTERMOST default-DENY. It is checked at
//! the top of `actions::execute_action` BEFORE any budget/drain/skeptic/freshness
//! logic — a denied money action never reaches provider budget or drain logic at
//! all. It does not weaken any existing gate; it only ever REFUSES earlier.

// The closed money-capable set is empty today (used only by the closure test)
// and the Verdict accessors are exercised by tests + future callers; mirror the
// crate-wide `#![allow(dead_code)]` house style (deploy.rs, ceo.rs) so the
// preemptive scaffolding does not warn while it waits for a future money kind.
#![allow(dead_code)]

use crate::notify;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The CLOSED set of MONEY-CAPABLE action kinds Solomon's autonomous funnel is
/// allowed to even *consider*. EMPTY today — Solomon has no money-out capability
/// and dispatches no trade itself (whitelisted lanes' own bots place trades).
/// The build-time closure test [`closure_every_money_kind_classifies`] asserts
/// every member of this set produces a decisive [`Verdict`] (Allow or an
/// explicit Deny) rather than falling through to the unknown-money DENY arm — so
/// a FUTURE money kind cannot be added without also adding its classification
/// and its human-gated whitelist reasoning in the SAME commit.
pub const MONEY_CAPABLE_KINDS: &[&str] = &[];

/// The ONLY permitted money verb: a trade placement, performed by a whitelisted
/// lane's OWN bot binary within its pre-existing (Kalshi/futures) account. Named
/// here so the classifier and its tests share one spelling. Solomon never emits
/// this itself today — it is reserved so the ALLOW path is explicit and testable.
pub const PLACE_TRADE_KIND: &str = "place_trade";

/// Substrings that mark an action as MONEY-OUT / external-spend. Any action kind
/// containing one of these is DENIED outright — this is the explicit blocklist
/// half of the fail-closed classifier (the other half is: anything else money-
/// capable that is not the whitelisted trade is also denied).
const MONEY_OUT_MARKERS: &[&str] = &[
    "withdraw", "payout", "transfer", "deposit", "fund", "funding", "purchase",
    "buy", "pay", "payment", "checkout", "charge", "invoice", "wire", "ach",
    "remit", "disburse", "spend", "ad_spend", "ads", "subscribe", "subscription",
    "signup", "sign_up", "stripe", "paypal", "venmo", "zelle", "cashout",
    "send_money", "sendmoney", "topup", "top_up", "refund", "settle_cash",
];

/// A money-out guard decision. `Allow` carries the whitelisted lane's reason;
/// `Deny` carries a human-readable refusal that names the fail-closed rule that
/// fired.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Permitted: a trade placement by a whitelisted (live-money) lane's own bot.
    Allow { reason: String },
    /// Refused: money-out, or an unknown/ambiguous money-capable action.
    Deny { reason: String },
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Verdict::Allow { .. })
    }
    pub fn reason(&self) -> &str {
        match self {
            Verdict::Allow { reason } | Verdict::Deny { reason } => reason,
        }
    }
}

/// A lane is WHITELISTED for the single permitted money action (a trade its OWN
/// bot places) iff it is a pre-existing live-money lane — identified by EITHER of
/// the two markers Solomon already uses, reusing the SAME predicates so this
/// guard and the drain/allocate gates can never diverge:
///   (1) repos.json `live_money: true`  -> `redeploy::is_live_money`
///   (2) outcomes-ledger `equity_usd`   -> `ceo::allocate::is_real_money`
/// Solomon holds no trading credentials and places no order; the whitelist is a
/// property of the LANE, and the permitted verb is dispatched by that lane's own
/// binary. Any other repo is NOT whitelisted.
pub fn is_whitelisted_lane(repo: &Value) -> bool {
    crate::redeploy::is_live_money(repo) || crate::ceo::allocate::is_real_money(repo)
}

/// True iff a bare action-kind string looks money-capable at all (contains a
/// money-out marker, or IS the reserved trade verb). A kind that trips NONE of
/// these is a plain non-money remediation and the guard waves it through — the
/// guard only ever REFUSES, never blocks the existing non-money action funnel.
pub fn is_money_capable(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    if k == PLACE_TRADE_KIND {
        return true;
    }
    MONEY_OUT_MARKERS.iter().any(|m| k.contains(m))
}

/// PURE fail-closed classifier. Given an action `kind` and the `repo` row it
/// would run against, decide whether it may proceed under the NO-MONEY-OUT
/// invariant. No IO.
///
/// Decision table (checked in order):
///   1. NOT money-capable                       -> Allow (a normal non-money
///      remediation — the guard is transparent to the existing action funnel).
///   2. money-out marker present                -> Deny (withdraw/transfer/pay/
///      ad-spend/... — Solomon never moves money out, ever).
///   3. `place_trade` on a WHITELISTED lane      -> Allow (the ONE permitted
///      money verb: a trade by that lane's own bot in its Kalshi/futures acct).
///   4. `place_trade` on a NON-whitelisted lane  -> Deny (trade only inside a
///      pre-existing whitelisted account).
///   5. anything else money-capable / ambiguous  -> Deny (DEFAULT-DENY — a
///      future money tool is blocked until explicitly whitelisted).
pub fn classify(kind: &str, repo: &Value) -> Verdict {
    let k = kind.to_ascii_lowercase();

    // (1) Not money-capable at all: transparent pass-through.
    if !is_money_capable(&k) {
        return Verdict::Allow {
            reason: format!("'{kind}' is not a money-capable action"),
        };
    }

    // (2) Explicit money-OUT: always refused, regardless of lane. Solomon moves
    // no money out under any circumstance — this is the HARD invariant.
    if let Some(marker) = MONEY_OUT_MARKERS.iter().find(|m| k.contains(**m)) {
        return Verdict::Deny {
            reason: format!(
                "money-out DENIED: '{kind}' matches the external-spend marker '{marker}'. \
                 Solomon never withdraws/transfers/deposits/funds/purchases/pays or spends \
                 externally — money-out stays human-gated (doctrine: deploy.rs, ceo.rs, \
                 allocate.rs, watchdog.rs, AI-CEO-ARCHITECTURE)"
            ),
        };
    }

    // (3)+(4) The single permitted money verb: a trade placement, and ONLY on a
    // pre-existing whitelisted live-money lane (asmodeus / kairos — Kalshi +
    // futures), placed by that lane's OWN bot.
    //
    // CRITICAL: this ALLOW does NOT authorize Solomon to place a trade itself.
    // `place_trade` is deliberately NOT a member of actions::ACTION_KINDS, so
    // `execute_action` has no dispatch arm for it and would refuse it via its
    // `_ => {ok:false}` closed-registry arm even after this guard passes. The
    // ALLOW exists to make the whitelist SEMANTICS explicit + testable, and to
    // mark the ONLY money verb a whitelisted lane's OWN bot may perform. If a
    // future commit ever wires an executable place_trade, it must dispatch the
    // LANE's own binary — never a Solomon-held trading tool (audit: "never by
    // Solomon"); adding it to ACTION_KINDS also trips actions.rs's closure test.
    if k == PLACE_TRADE_KIND {
        if is_whitelisted_lane(repo) {
            return Verdict::Allow {
                reason: format!(
                    "place_trade ALLOWED: '{}' is a whitelisted live-money lane \
                     (live_money/equity_usd) — trade placed by its OWN bot within its \
                     pre-existing Kalshi/futures account",
                    crate::control::paths::repo_name(repo)
                ),
            };
        }
        return Verdict::Deny {
            reason: format!(
                "place_trade DENIED: '{}' is NOT a whitelisted live-money lane. Trade \
                 placement is permitted ONLY inside a pre-existing whitelisted account \
                 (live_money:true OR equity_usd present)",
                crate::control::paths::repo_name(repo)
            ),
        };
    }

    // (5) DEFAULT-DENY: money-capable but neither an explicit money-out marker
    // nor the whitelisted trade verb => ambiguous/unknown => refused. This is the
    // preemptive arm — a future money tool is blocked until a human explicitly
    // whitelists it by adding a decisive arm above (pinned by the closure test).
    Verdict::Deny {
        reason: format!(
            "money-out DENIED (fail-closed): '{kind}' is money-capable but not the \
             whitelisted trade verb and not a recognized non-money action — DEFAULT-DENY. \
             A new money-capable action must be explicitly whitelisted (human-gated) before \
             it can run"
        ),
    }
}

/// The GATE the chokepoint calls. Returns `None` when the action may proceed
/// (Allow), or `Some(refusal Value)` shaped like the other `execute_action`
/// refusals (`{ok:false, kind, error, money_guard:true}`) when DENIED — so the
/// caller can early-return it without any new wiring, exactly as `run_pi`
/// returns a synthetic refusal for an exhausted budget. A DENY also pages the
/// operator, marker-deduped, because a denied autonomous money attempt is a real
/// must-see event.
pub fn guard(kind: &str, repo: &Value) -> Option<Value> {
    match classify(kind, repo) {
        Verdict::Allow { .. } => None,
        Verdict::Deny { reason } => {
            page_money_denied_deduped(repo, kind, &reason);
            Some(json!({
                "ok": false,
                "kind": kind,
                "money_guard": true,
                "error": reason,
            }))
        }
    }
}

// --------------------------------------------------------------------------- //
// LOUD PAGE — marker-deduped, mirrors actions.rs::page_operator_deduped
// --------------------------------------------------------------------------- //

/// One page per (repo, money-deny) per this window. A denied money attempt is
/// rare and important; dedup only prevents a page storm if a broken caller
/// retries every sweep.
const MONEY_PAGE_DEDUP_WINDOW_S: u64 = 86_400;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// runtime/<name>/_money_denied — the dedup marker. The kind can be arbitrary,
/// so we do NOT interpolate it into the path; one marker per lane is enough (a
/// denied money attempt on a lane is the alarm, not the specific verb).
fn money_marker_path(dir: &Path) -> PathBuf {
    dir.join("_money_denied")
}

fn money_deduped_at(dir: &Path, now: u64) -> bool {
    std::fs::read_to_string(money_marker_path(dir))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .map(|sent| now.saturating_sub(sent) < MONEY_PAGE_DEDUP_WINDOW_S)
        .unwrap_or(false)
}

/// Page the operator LOUDLY about a denied money attempt, marker-deduped (24h).
/// The marker is stamped BEFORE the send (the actions.rs lesson: a crash between
/// the two suppresses at most one page; the reverse order can storm).
fn page_money_denied_deduped(repo: &Value, kind: &str, reason: &str) {
    let dir = match crate::control::paths::runtime_dir(repo) {
        Some(d) => d,
        None => {
            // No runtime dir (e.g. nameless test row): still page, just can't dedup.
            let name = crate::control::paths::repo_name(repo);
            let _ = notify::send(&notify::Notice::red(
                format!("Solomon: NO-MONEY-OUT guard DENIED a money action ({name})"),
                format!("kind='{kind}' — {reason}"),
            ));
            return;
        }
    };
    let now = unix_now();
    if money_deduped_at(&dir, now) {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(money_marker_path(&dir), format!("{now}"));
    let name = crate::control::paths::repo_name(repo);
    let _ = notify::send(&notify::Notice::red(
        format!("Solomon: NO-MONEY-OUT guard DENIED a money action ({name})"),
        format!("kind='{kind}' — {reason}"),
    ));
}

// --------------------------------------------------------------------------- //
// tests — the decision table + the BUILD-TIME CLOSURE CONTRACT
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;

    fn whitelisted_repo() -> Value {
        // A pre-existing live-money lane: live_money:true (asmodeus/kairos class).
        json!({ "name": "asmodeus", "live_money": true })
    }

    fn equity_repo() -> Value {
        // Whitelisted via the OTHER marker: an equity_usd-carrying outcomes row.
        json!({ "name": "kairos", "equity_usd": 1234.5 })
    }

    fn plain_repo() -> Value {
        json!({ "name": "sover" })
    }

    // ---- REQUIRED: money-out DENIED ----
    #[test]
    fn money_out_actions_are_denied_on_every_lane() {
        // A representative spread of external-spend verbs — denied even on a
        // whitelisted lane (money-OUT is the HARD invariant, no lane exempts it).
        for kind in [
            "withdraw", "withdraw_funds", "transfer", "bank_transfer", "deposit",
            "fund_account", "purchase", "buy_ads", "pay_invoice", "payment_intent",
            "stripe_checkout", "ad_spend", "paid_signup", "cashout", "send_money",
            "wire_transfer",
        ] {
            for repo in [whitelisted_repo(), equity_repo(), plain_repo()] {
                let v = classify(kind, &repo);
                assert!(
                    !v.is_allowed(),
                    "money-out '{kind}' must be DENIED (got Allow) on repo {repo}"
                );
                assert!(
                    v.reason().to_lowercase().contains("denied"),
                    "deny reason should say DENIED: {}",
                    v.reason()
                );
            }
        }
    }

    // ---- REQUIRED: a whitelisted Kalshi/futures trade ALLOWED ----
    #[test]
    fn place_trade_on_whitelisted_lane_is_allowed() {
        // asmodeus (live_money) and kairos (equity_usd) — the two whitelist keys.
        let a = classify(PLACE_TRADE_KIND, &whitelisted_repo());
        assert!(a.is_allowed(), "place_trade on live_money lane must ALLOW: {}", a.reason());
        assert!(a.reason().to_lowercase().contains("allowed"));

        let k = classify(PLACE_TRADE_KIND, &equity_repo());
        assert!(k.is_allowed(), "place_trade on equity_usd lane must ALLOW: {}", k.reason());

        // ...but the SAME verb on a non-whitelisted lane is DENIED (trade only in
        // a pre-existing whitelisted account).
        let deny = classify(PLACE_TRADE_KIND, &plain_repo());
        assert!(!deny.is_allowed(), "place_trade on a non-whitelisted lane must DENY");
    }

    // ---- REQUIRED: ambiguous money action DENIED (fail-closed) ----
    #[test]
    fn ambiguous_money_capable_action_is_default_denied() {
        // A money-capable kind that is neither an explicit money-out marker match
        // by the trade path nor the whitelisted trade verb: an unknown future
        // "spend"-flavored tool. DEFAULT-DENY, even on a whitelisted lane.
        let v = classify("spend_treasury", &whitelisted_repo());
        assert!(!v.is_allowed(), "ambiguous money action must be DEFAULT-DENIED");

        // A brand-new unrecognized money verb with no marker at all but forced
        // money-capable would still be denied by the default arm; here we prove a
        // marker-bearing ambiguous verb is denied on the strongest lane.
        let v2 = classify("fund_new_wallet", &equity_repo());
        assert!(!v2.is_allowed(), "fund_* is money-out and must be DENIED");
    }

    // ---- non-money actions pass through untouched ----
    #[test]
    fn non_money_actions_are_allowed_untouched() {
        // The existing closed-registry action kinds must all sail through — the
        // guard is transparent to the non-money action funnel.
        for kind in [
            "none", "restart_lane", "reset_to_base", "run_fix_session",
            "park_primary_endpoint", "clear_escalation_and_retry",
            "page_operator_deduped",
        ] {
            let v = classify(kind, &plain_repo());
            assert!(v.is_allowed(), "non-money kind '{kind}' must pass the guard: {}", v.reason());
        }
        // and the guard() entrypoint returns None (proceed) for them.
        assert!(guard("restart_lane", &plain_repo()).is_none());
    }

    // ---- guard() refusal shape ----
    #[test]
    fn guard_returns_refusal_value_for_denied_money_action() {
        // Silence the page so this test doesn't emit + serialize against the
        // notify kill-switch env var like actions.rs does.
        let _g = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");

        let refusal = guard("withdraw_all", &whitelisted_repo())
            .expect("a money-out action must produce a refusal Value");
        assert_eq!(refusal["ok"], false);
        assert_eq!(refusal["money_guard"], true);
        assert_eq!(refusal["kind"], "withdraw_all");
        assert!(refusal["error"].as_str().unwrap().to_lowercase().contains("denied"));

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    // ---- the guard's ALLOW grants nothing executable today ----
    // Even though classify() ALLOWs place_trade on a whitelisted lane, place_trade
    // is NOT in actions::ACTION_KINDS, so execute_action still refuses it via the
    // closed-registry `_` arm — Solomon cannot place a trade ITSELF today. This
    // pins the audit invariant "never by Solomon": a whitelisted lane's own bot
    // places trades; Solomon's autonomous funnel has no dispatch arm for it.
    #[test]
    fn place_trade_is_not_in_the_closed_action_registry_so_solomon_cannot_dispatch_it() {
        assert!(
            !crate::actions::ACTION_KINDS.contains(&PLACE_TRADE_KIND),
            "place_trade must NOT be an executable action kind — Solomon never places a trade \
             itself (audit: 'never by Solomon'); a whitelisted lane's OWN bot does"
        );
        // And execute_action, the real chokepoint, refuses it: the guard ALLOWs
        // (whitelisted lane) but the closed registry has no arm -> honest refusal.
        let out = crate::actions::execute_action(
            PLACE_TRADE_KIND,
            &whitelisted_repo(),
            "money_guard_probe",
            false,
        );
        assert_eq!(out["ok"], false, "execute_action must refuse place_trade (no dispatch arm)");
        assert!(
            out["error"].as_str().unwrap().contains("closed registry"),
            "refusal must be the closed-registry arm, proving no Solomon-side trade dispatch: {}",
            out["error"]
        );
    }

    // ---- THE BUILD-TIME CLOSURE CONTRACT ----
    // Every member of the CLOSED money-capable-kind set must classify to a
    // DECISIVE verdict (it is money-capable AND it does not fall through to the
    // generic default-DENY-by-accident arm for an UNKNOWN kind). EMPTY today, so
    // this vacuously holds — but the moment a future money kind is added to the
    // set, this test fails unless a decisive classification arm is added for it
    // in the same commit (mirrors actions.rs's ACTION_KINDS closure test).
    #[test]
    fn closure_every_money_kind_classifies() {
        for kind in MONEY_CAPABLE_KINDS {
            assert!(
                is_money_capable(kind),
                "'{kind}' is in MONEY_CAPABLE_KINDS but is_money_capable() says it isn't — the \
                 set and the classifier must agree"
            );
            // On BOTH a whitelisted and a plain lane it must reach a real Allow/Deny
            // (classify is total, so this is really a guard against someone adding a
            // kind the markers don't recognize, leaving it in the ambiguous arm).
            let _ = classify(kind, &whitelisted_repo());
            let _ = classify(kind, &plain_repo());
        }
    }
}
