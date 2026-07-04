//! Deep-work FOCUS allocator (Polsia): concentrate sustained, multi-step campaign work on ONE
//! top-leverage project at a time, while the per-lane baseline (ops-RED graft, crash recovery,
//! hygiene, the daily ceo goal) keeps every OTHER project alive so nothing rots. Rides the
//! every-sweep `ceo::tick`.
//!
//! Sticky by design: the focus does NOT hop on every rank shuffle (that would strand half-finished
//! campaigns). It rotates only when the current focus stops being a candidate (went red / unhealthy /
//! real-money) or after a hard `MAX_FOCUS_DAYS` anti-starvation cap. The focus lane's next milestone
//! is decomposed (one LLM call, at most once/day) into ordered `[campaign:<slug>]` steps prepended
//! atop its backlog; the priority picker (`backlog::top_backlog_item`, rank 1) then drives them in
//! order — each at the deep tier's budget — so successive one-shot iterations CHAIN into real deep
//! work. Green-before-growth is inherited: `pick_focus` only ever returns a green, non-real-money
//! lane, so a broken engine (e.g. a RED sover) is fixed by the ops-RED graft, never "focused".

use crate::control::{paths, proc};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Anti-starvation cap: rotate focus off a lane after this many days even if it stays top-ranked, so
/// no single lane monopolizes the deep-work allocation.
const MAX_FOCUS_DAYS: i64 = 2;
/// A decomposition must yield at least this many steps to count as a campaign (else the lane just uses
/// its normal backlog); capped above at CAMPAIGN_MAX_STEPS ordered steps.
const CAMPAIGN_MIN_STEPS: usize = 2;
const CAMPAIGN_MAX_STEPS: usize = 7;

/// HERE/runtime/_focus.json — the persisted deep-work focus + its campaign.
/// Shape: {"lane","since"(date),"slug","milestone","planned"(date)}.
fn focus_path() -> PathBuf {
    paths::here().join("runtime").join("_focus.json")
}

fn read_focus() -> Value {
    std::fs::read(focus_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_focus(v: &Value) {
    if let Some(parent) = focus_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(&focus_path(), v);
}

/// The deep-work focus lane for this sweep (PURE — unit-tested). Sticky: keeps the current focus while
/// it is still a valid candidate and under the day cap, so a campaign isn't stranded by a transient
/// rank shuffle.
///   - no current focus            -> adopt `top` (the highest-leverage candidate, or None -> hold).
///   - current no longer candidate -> switch to `top` (its engine broke; move the effort).
///   - held >= max_focus_days AND a DIFFERENT top exists -> rotate to `top` (anti-starvation).
///   - otherwise                   -> keep the current focus.
pub fn next_focus(
    current: Option<&str>,
    top: Option<&str>,
    current_still_candidate: bool,
    since_days: i64,
    max_focus_days: i64,
) -> Option<String> {
    match current {
        None => top.map(str::to_string),
        Some(cur) => {
            if !current_still_candidate {
                return top.map(str::to_string);
            }
            if since_days >= max_focus_days {
                if let Some(t) = top {
                    if t != cur {
                        return Some(t.to_string());
                    }
                }
            }
            Some(cur.to_string())
        }
    }
}

/// Render ordered campaign backlog lines for `slug` (PURE — unit-tested). Each carries a real deep
/// tier (so `tier_budget` lifts its reasoning/timeout) AND the `[campaign:<slug>]` rank + idempotence
/// marker, plus a `(campaign <date>)` provenance tag. Returned in step order (step 1 first); the
/// caller prepends the joined block so step 1 lands on top of the backlog.
pub fn campaign_lines(slug: &str, steps: &[String], tier: &str, today: &str) -> Vec<String> {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "- [ ] [{tier}] [campaign:{slug}] (step {}) {} (campaign {today})",
                i + 1,
                s.trim()
            )
        })
        .collect()
}

/// Count OPEN (`- [ ]`) backlog lines belonging to `slug` (PURE — unit-tested). Zero == the campaign
/// is drained, the trigger to plan the next milestone.
pub fn open_campaign_steps(existing: &str, slug: &str) -> usize {
    let marker = format!("[campaign:{slug}]");
    existing
        .lines()
        .filter(|l| l.trim().starts_with("- [ ]") && l.contains(&marker))
        .count()
}

/// Whole days between two "%Y-%m-%d" dates (`today - since`), or 0 on absent/unparseable input.
fn days_since(since: Option<&str>, today: &str) -> i64 {
    let t = match chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    match since.and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()) {
        Some(d) => (t - d).num_days(),
        None => 0,
    }
}

/// The every-sweep focus tick, wired into `ceo::tick`. Deterministic focus selection (cheap, every
/// sweep) + a bounded once/day campaign decomposition (one LLM call) for the focus lane. catch_unwind
/// is the caller's responsibility (the tick wraps this) so a focus failure never aborts the sweep.
pub fn maybe_focus(snapshot: &Value, status: &Value) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // 1. Rank + the top candidate. pick_focus returns ONLY a green, non-real-money, positive-leverage
    //    lane (green-before-growth is inherited — a red/unhealthy lane never becomes the focus).
    let ranking = crate::ceo::allocate::rank_lanes(snapshot, status);
    let top = crate::ceo::allocate::pick_focus(&ranking);

    // 2. Current focus + how long it has been held.
    let st = read_focus();
    let current = st.get("lane").and_then(|v| v.as_str());
    let since = st.get("since").and_then(|v| v.as_str());
    let since_days = days_since(since, &today);
    let current_still_candidate = current
        .map(|c| ranking.iter().any(|(n, s, _)| n == c && *s > 0.0))
        .unwrap_or(false);

    // 3. Decide the focus lane. None -> no candidate -> clear a stale focus + hold the fleet.
    let focus = match next_focus(
        current,
        top.as_deref(),
        current_still_candidate,
        since_days,
        MAX_FOCUS_DAYS,
    ) {
        Some(f) => f,
        None => {
            if current.is_some() {
                write_focus(&json!({}));
            }
            return;
        }
    };

    // 4. Carry or reset the campaign fields. A focus CHANGE resets slug/planned/milestone/since; a
    //    same-lane sweep preserves them.
    let changed = current != Some(focus.as_str());
    let since_out = if changed {
        today.clone()
    } else {
        since.unwrap_or(&today).to_string()
    };
    let mut slug = if changed {
        String::new()
    } else {
        st.get("slug").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let mut planned = if changed {
        String::new()
    } else {
        st.get("planned").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let mut milestone = if changed {
        String::new()
    } else {
        st.get("milestone").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };

    // 5. Campaign lifecycle: when the campaign is DRAINED (0 open steps for its slug) AND we have not
    //    planned today, decompose the next milestone into ordered steps and prepend them.
    let backlog_path = super::backlog_path(&focus);
    let existing = std::fs::read_to_string(&backlog_path).unwrap_or_default();
    let open = if slug.is_empty() {
        0
    } else {
        open_campaign_steps(&existing, &slug)
    };
    if open == 0 && planned != today {
        // ONE decomposition attempt per day (success or fail) so a persistent LLM failure never
        // hammers the endpoint every 2-min sweep — the lane just works its normal backlog that day.
        planned = today.clone();
        if let Some((ms, tier, steps)) = decompose_campaign(&focus, snapshot) {
            let new_slug = format!("{focus}-{today}");
            let block = campaign_lines(&new_slug, &steps, &tier, &today).join("\n");
            // Prepend atop the backlog, step 1 on top; reuse the atomic + read-error-skip contract (a
            // READ ERROR — the improver mid-rewrite — skips rather than risk truncating a live file).
            match std::fs::read_to_string(&backlog_path) {
                Ok(cur) => {
                    if let Some(parent) = backlog_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if proc::atomic_write_bytes(&backlog_path, format!("{block}\n{cur}").as_bytes()).is_ok() {
                        slug = new_slug;
                        milestone = ms.clone();
                        let _ = crate::notify::send(&crate::notify::Notice::report(
                            format!("Solomon: deep-work focus -> {focus}"),
                            format!("milestone: {ms} ({} steps)", steps.len()),
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(parent) = backlog_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if proc::atomic_write_bytes(&backlog_path, format!("{block}\n").as_bytes()).is_ok() {
                        slug = new_slug;
                        milestone = ms;
                    }
                }
                Err(_) => {} // read failed mid-rewrite — skip this sweep, retry tomorrow
            }
        }
    }

    // 6. Persist the focus state (atomic).
    write_focus(&json!({
        "lane": focus,
        "since": since_out,
        "slug": slug,
        "milestone": milestone,
        "planned": planned,
    }));
}

/// One LLM call to decompose the focus lane's NEXT milestone into ordered, each-shippable steps.
/// Returns (milestone, tier, steps) or None on any failure / too-few steps (the lane then just uses
/// its normal backlog). Reuses the CEO planner model + JSON extraction + goal-post resolution.
fn decompose_campaign(name: &str, snapshot: &Value) -> Option<(String, String, Vec<String>)> {
    // north star: the operator's committed goal.md line, else the repos.json goal.
    let repo = crate::control::registry::read_repos_json()
        .into_iter()
        .find(|r| paths::repo_name(r) == name)?;
    let fallback = repo.get("goal").and_then(|v| v.as_str()).unwrap_or("");
    let goal_md = std::fs::read_to_string(super::goal_post_path(name)).ok();
    let north_star = super::pick_goal_post(goal_md.as_deref(), fallback);
    if north_star.trim().is_empty() {
        return None;
    }
    let outcomes = snapshot
        .get("projects")
        .and_then(|p| p.get(name))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let velocity = super::velocity_context(&outcomes, &north_star);

    let system = "You are Solomon's deep-work planner. Given ONE project's north-star goal, its \
        measured 24h outcomes, and its velocity, choose the project's NEXT concrete milestone toward \
        that north star and DECOMPOSE it into 3-7 ORDERED steps — each independently shippable by a \
        coding agent in ONE iteration and verifiable from files/tests/logs, where each later step may \
        build on the earlier ones having landed. Prefer real feature / scaling / quality work that \
        moves the north-star metric FORWARD, never cosmetic busywork; NEVER weaken safety, \
        kill-switch, breaker, or capital-guard mechanisms. Reply with STRICT JSON only: \
        {\"milestone\": \"<one sentence>\", \"tier\": \"feature|architecture\", \
        \"steps\": [\"<step 1>\", \"<step 2>\", \"<step 3>\"]} — 3 to 7 ordered steps.";
    let user = serde_json::to_string_pretty(&json!({
        "lane": name,
        "north_star": north_star,
        "outcomes_24h": outcomes,
        "velocity": velocity,
    }))
    .unwrap_or_default();

    let reply = super::ollama_chat(super::CEO_MODEL, system, &user).ok()?;
    let parsed = super::extract_json(&reply)?;
    let milestone = super::cap_line(
        parsed.get("milestone").and_then(|v| v.as_str()).unwrap_or(""),
        200,
    );
    let tier = match parsed.get("tier").and_then(|v| v.as_str()) {
        Some("architecture") => "architecture",
        _ => "feature",
    }
    .to_string();
    let steps: Vec<String> = parsed
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .map(|s| super::cap_line(s, 300))
                .filter(|s| !s.is_empty())
                .take(CAMPAIGN_MAX_STEPS)
                .collect()
        })
        .unwrap_or_default();
    if steps.len() < CAMPAIGN_MIN_STEPS {
        return None; // too few to be a campaign — the lane uses its normal backlog
    }
    Some((milestone, tier, steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_focus_adopts_top_when_none() {
        assert_eq!(next_focus(None, Some("sover"), false, 0, 2).as_deref(), Some("sover"));
        assert_eq!(next_focus(None, None, false, 0, 2), None);
    }

    #[test]
    fn next_focus_is_sticky_while_candidate_and_under_cap() {
        // current still a candidate, under the cap, even though a DIFFERENT lane is top -> KEEP it
        // (a transient rank shuffle must not strand a half-finished campaign).
        assert_eq!(next_focus(Some("sover"), Some("dotz"), true, 1, 2).as_deref(), Some("sover"));
    }

    #[test]
    fn next_focus_switches_when_current_not_candidate() {
        // current engine broke (no longer a positive-leverage candidate) -> move to top.
        assert_eq!(next_focus(Some("sover"), Some("dotz"), false, 1, 2).as_deref(), Some("dotz"));
        // ...and with no top either, hold the fleet.
        assert_eq!(next_focus(Some("sover"), None, false, 1, 2), None);
    }

    #[test]
    fn next_focus_rotates_after_cap_only_to_a_different_top() {
        // held past the cap AND a different top exists -> rotate (anti-starvation).
        assert_eq!(next_focus(Some("sover"), Some("dotz"), true, 2, 2).as_deref(), Some("dotz"));
        // held past the cap but current IS still the top -> stay (nowhere better to go).
        assert_eq!(next_focus(Some("sover"), Some("sover"), true, 5, 2).as_deref(), Some("sover"));
    }

    #[test]
    fn campaign_lines_are_ordered_tagged_and_marked() {
        let steps = vec!["do A".to_string(), "do B".to_string()];
        let lines = campaign_lines("sover-2026-07-04", &steps, "feature", "2026-07-04");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("- [ ] [feature] [campaign:sover-2026-07-04] (step 1) do A"));
        assert!(lines[0].ends_with("(campaign 2026-07-04)"));
        assert!(lines[1].contains("(step 2) do B"));
        // the picker ranks a campaign step at bucket 1, and strip_tier yields the real (deep) tier.
        assert_eq!(crate::improver::backlog::backlog_item_rank(&lines[0][5..]), 1);
        assert_eq!(crate::improver::backlog::strip_tier(lines[0][5..].trim()).1, "feature");
    }

    #[test]
    fn open_campaign_steps_counts_open_matching_slug_only() {
        let bl = "- [ ] [feature] [campaign:sc] (step 1) a\n\
                  - [x] [feature] [campaign:sc] (step 2) done\n\
                  - [ ] [feature] [campaign:other] (step 1) b\n\
                  - [ ] plain\n";
        assert_eq!(open_campaign_steps(bl, "sc"), 1); // only the OPEN sc step
        assert_eq!(open_campaign_steps(bl, "none"), 0);
    }

    #[test]
    fn days_since_computes_or_zero() {
        assert_eq!(days_since(Some("2026-07-02"), "2026-07-04"), 2);
        assert_eq!(days_since(Some("2026-07-04"), "2026-07-04"), 0);
        assert_eq!(days_since(None, "2026-07-04"), 0);
        assert_eq!(days_since(Some("garbage"), "2026-07-04"), 0);
    }
}
