//! Config-provenance tripwire + controller-clean preflight — countermeasure to failure-catalog #6
//! ("the control plane exempts itself from its own gates"; see docs/rsi/PROVENANCE.md).
//!
//! Watched config: repos.json / ops.json / actions.json (git-tracked) and .env (gitignored —
//! secrets). A mutation that persists uncommitted past a grace window pages the operator ONCE
//! (marker-deduped) and writes a TTL'd `HOLD_META` into every trading-adjacent lane's runtime dir;
//! WS1's `freshness::short_circuit` honors the hold, so meta-decisions halt WITHOUT a dead end
//! (requirement 7: the hold self-expires if this tripwire stops running). Committing with an
//! `operator:`/`rsi:` provenance prefix — or reverting — clears the holds and page markers.
//! Deliberately no auto-commit: an unattributed mutation is evidence of an ungated write path;
//! the job is to stop and surface it, never to launder it into history.
//!
//! `controller_clean` is the preflight for catalog #6's other half: Solomon refusing to meta-work
//! its OWN repo from a dirty or off-base tree (the 601-uncommitted-lines incident).

use crate::control::{paths, proc, registry};
use crate::notify;
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Git-tracked watched files, relative to the controller repo root.
const TRACKED: &[&str] = &["repos.json", "ops.json", "actions.json"];
/// The gitignored secrets file — watched by content hash, never by git.
const ENV_FILE: &str = ".env";
/// Drift may persist this long uncommitted before the tripwire pages + holds (the acceptance
/// criterion is ">10 min", so the page fires on the first sweep after 600s).
const DRIFT_GRACE_S: f64 = 600.0;
/// HOLD_META TTL — the degraded-mode expiry freshness::hold_meta_short_circuit enforces.
const HOLD_TTL_S: f64 = 3600.0;

fn iso_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One tripwire pass over the watched files. Called from the watchdog sweep (GUI tick + Sentinel).
/// Returns `{"ts", "actions": [str], "files": {...}}` — actions only on TRANSITIONS (paged,
/// adopted, cleared), never per-sweep noise (catalog #8: page-flood desensitization).
pub fn check() -> Value {
    let rows = registry::load_repos();
    check_impl(
        paths::here(),
        &paths::here().join("runtime"),
        &rows,
        unix_now(),
    )
}

/// The trading-adjacent lanes: any repos.json row with `live_app == true` OR a `tiers` key
/// (kairos/asmodeus/sover class — lanes where an unversioned config mutation can move money).
fn held_lanes(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter(|r| {
            r.get("live_app").and_then(Value::as_bool).unwrap_or(false)
                || r.get("tiers").is_some()
        })
        .filter_map(|r| r.get("name").and_then(Value::as_str))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// Tracked-file drift: `git diff --name-only HEAD -- <file>` non-empty. `None` on any git failure
/// (indeterminate must never page — a broken git is its own, separately-surfaced problem).
fn git_dirty(root: &Path, file: &str) -> Option<bool> {
    let root_s = root.to_string_lossy().into_owned();
    let r = proc::run(
        &["git", "-C", &root_s, "diff", "--name-only", "HEAD", "--", file],
        None,
        Some(Duration::from_secs(60)),
    )
    .ok()?;
    if r.code != 0 {
        return None;
    }
    Some(!r.stdout.trim().is_empty())
}

fn load_state(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("files").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn save_state(path: &Path, files: &Map<String, Value>) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(&json!({"files": files})).unwrap_or_default(),
        )
    })();
}

fn marker_path(runtime_root: &Path, file: &str) -> PathBuf {
    runtime_root.join(format!("_config_drift_paged_{file}"))
}

/// Remove the page marker + every lane HOLD_META that names `file` (holds for OTHER drifted files
/// are left in place). Scans all runtime subdirs, not just current rows, so a lane removed from
/// repos.json mid-drift still gets unheld.
fn clear_drift_artifacts(runtime_root: &Path, file: &str) {
    let _ = std::fs::remove_file(marker_path(runtime_root, file));
    if let Ok(rd) = std::fs::read_dir(runtime_root) {
        for e in rd.flatten() {
            let hold = e.path().join("HOLD_META");
            if let Ok(text) = std::fs::read_to_string(&hold) {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if v.get("file").and_then(Value::as_str) == Some(file) {
                    let _ = std::fs::remove_file(&hold);
                }
            }
        }
    }
}

fn write_holds(runtime_root: &Path, lanes: &[String], file: &str, now: f64) {
    for lane in lanes {
        let dir = runtime_root.join(lane);
        let _ = std::fs::create_dir_all(&dir);
        let hold = json!({
            "reason": format!("unversioned config mutation: {file}"),
            "file": file,
            "ts": iso_now(),
            "expires_at": now + HOLD_TTL_S,
        });
        let _ = std::fs::write(dir.join("HOLD_META"), hold.to_string());
    }
}

/// Path/clock-parameterized core of [`check`] so the tests drive tmp git repos + synthetic time.
fn check_impl(repo_root: &Path, runtime_root: &Path, rows: &[Value], now: f64) -> Value {
    let state_path = runtime_root.join("_config_provenance.json");
    let mut files = load_state(&state_path);
    let mut actions: Vec<String> = Vec::new();
    let lanes = held_lanes(rows);

    let mut watched: Vec<&str> = TRACKED.to_vec();
    watched.push(ENV_FILE);
    for name in watched {
        let cur_sha = std::fs::read(repo_root.join(name))
            .map(|b| sha256_hex(&b))
            .unwrap_or_default();
        let entry = files
            .entry(name.to_string())
            .or_insert_with(|| json!({"sha256": "", "first_seen_drift": 0}));
        let baseline = entry
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut first = entry
            .get("first_seen_drift")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);

        let drift = if name == ENV_FILE {
            // First observation establishes the baseline — never a page on install.
            !baseline.is_empty() && cur_sha != baseline
        } else {
            git_dirty(repo_root, name) == Some(true)
        };

        if drift {
            if first == 0.0 {
                first = now; // stamp silently; the grace window starts here
            }
            let elapsed = now - first;
            if elapsed > DRIFT_GRACE_S {
                let marker = marker_path(runtime_root, name);
                if !marker.exists() {
                    let _ = notify::send(&notify::Notice::red(
                        format!("Solomon: unversioned config mutation — {name}"),
                        format!(
                            "{name} modified without a provenance commit for {}s. Trading-adjacent \
                             lanes are HELD ({}s TTL). Commit with an operator:/rsi: prefix or \
                             revert (docs/rsi/PROVENANCE.md).",
                            elapsed as i64, HOLD_TTL_S as i64
                        ),
                    ));
                    let _ = (|| -> std::io::Result<()> {
                        if let Some(p) = marker.parent() {
                            std::fs::create_dir_all(p)?;
                        }
                        std::fs::write(&marker, iso_now())
                    })();
                    actions.push(format!(
                        "config drift PAGED: {name} uncommitted > {}s — trading-adjacent lanes held",
                        DRIFT_GRACE_S as i64
                    ));
                }
                // Refresh the holds every sweep while the drift persists: the hold tracks the
                // drift, and the TTL only matters if this tripwire itself stops running.
                write_holds(runtime_root, &lanes, name, now);
                if name == ENV_FILE && elapsed > DRIFT_GRACE_S + HOLD_TTL_S {
                    // Secrets are never committed, so a persisting .env change has no commit path
                    // out — after the page + one full hold TTL the new content is ADOPTED as the
                    // baseline (requirement 7: degraded-mode fallback, never a dead end).
                    entry["sha256"] = json!(cur_sha);
                    first = 0.0;
                    clear_drift_artifacts(runtime_root, name);
                    actions.push(
                        ".env baseline adopted after hold TTL (secrets are never committed)"
                            .to_string(),
                    );
                }
            }
        } else {
            if first != 0.0 || marker_path(runtime_root, name).exists() {
                clear_drift_artifacts(runtime_root, name);
                actions.push(format!("config drift cleared: {name}"));
            }
            first = 0.0;
            entry["sha256"] = json!(cur_sha); // clean == the new baseline
        }
        entry["first_seen_drift"] = json!(first);
    }

    save_state(&state_path, &files);
    json!({"ts": iso_now(), "actions": actions, "files": files})
}

// --------------------------------------------------------------------------- //
// commit-tag convention (docs/rsi/PROVENANCE.md) — the checker that makes it enforced,
// not write-only (skeptic finding 4, 2026-07-06)
// --------------------------------------------------------------------------- //

/// True iff a commit subject satisfies the PROVENANCE.md tag convention for watched-file commits:
/// first token is `operator:` or an `rsi`-family tag (`rsi:`, or version-suffixed `rsi-vN...:`
/// e.g. `rsi-v3:` / `rsi-v3.1:`), followed by a space and a non-empty body. Merge commits are
/// excepted (git writes their subjects). Pure — this is the machine-readable form of the doc's
/// audit query; the repo-history test below runs it against the ACTUAL `git log` output so the
/// convention can never again be a write-only ledger.
pub fn valid_provenance_subject(subject: &str) -> bool {
    let s = subject.trim_start();
    if s.starts_with("Merge ") {
        return true;
    }
    if let Some(rest) = s.strip_prefix("operator: ") {
        return !rest.trim().is_empty();
    }
    let Some(after_rsi) = s.strip_prefix("rsi") else {
        return false;
    };
    // `rsi: ` or `rsi-vN[.M...]: ` — the version suffix must start `-v<digit>` and stay within
    // [0-9a-z.] (lowercase, rule 1 of the convention).
    let Some(tag_end) = after_rsi.find(": ") else {
        return false;
    };
    let suffix = &after_rsi[..tag_end];
    let body_ok = !after_rsi[tag_end + 2..].trim().is_empty();
    if suffix.is_empty() {
        return body_ok; // plain `rsi: `
    }
    let Some(ver) = suffix.strip_prefix("-v") else {
        return false;
    };
    body_ok
        && ver.starts_with(|c: char| c.is_ascii_digit())
        && ver
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '.')
}

// --------------------------------------------------------------------------- //
// controller-clean preflight
// --------------------------------------------------------------------------- //

/// Err when Solomon's OWN tree (paths::here()) is dirty (`git status --porcelain` non-empty,
/// runtime/ excluded — it is gitignored anyway), HEAD is not the repos.json solomon row's
/// pr_target_branch, or the base carries commits its upstream does not have (a weeks-stale
/// unpushed main is NOT clean — catalog #6's self-exemption; skeptic finding 4). The control
/// plane must PROVE it is clean before meta-work; an indeterminate git result is therefore also
/// an Err (a repo with NO upstream configured skips only the unpushed check — there is no remote
/// to be out-of-band with).
pub fn controller_clean() -> Result<(), String> {
    controller_clean_at(paths::here(), &solomon_base_branch())
}

fn solomon_base_branch() -> String {
    for r in registry::load_repos() {
        if r.get("name").and_then(Value::as_str) == Some("solomon") {
            return registry::project_pr_target_branch(&r);
        }
    }
    "main".to_string()
}

/// Path-parameterized core of [`controller_clean`] so tests drive clean/dirty/off-branch tmp repos.
fn controller_clean_at(root: &Path, expected_branch: &str) -> Result<(), String> {
    let root_s = root.to_string_lossy().into_owned();
    let st = proc::run(
        &["git", "-C", &root_s, "status", "--porcelain"],
        None,
        Some(Duration::from_secs(120)),
    )
    .map_err(|e| format!("git status failed: {e}"))?;
    if st.code != 0 {
        return Err(format!("git status exit {}: {}", st.code, st.stderr.trim()));
    }
    let dirty: Vec<&str> = st
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            // porcelain: "XY path" (path from col 3); belt-and-braces runtime/ exclusion.
            let p = l.get(3..).unwrap_or("").trim_start();
            !(p.starts_with("runtime/") || p.starts_with("runtime\\"))
        })
        .collect();
    if !dirty.is_empty() {
        let sample: Vec<&str> = dirty.iter().take(3).copied().collect();
        return Err(format!(
            "dirty tree: {} uncommitted path(s) [{}{}]",
            dirty.len(),
            sample.join(", "),
            if dirty.len() > 3 { ", ..." } else { "" }
        ));
    }
    let head = proc::run(
        &["git", "-C", &root_s, "rev-parse", "--abbrev-ref", "HEAD"],
        None,
        Some(Duration::from_secs(60)),
    )
    .map_err(|e| format!("git rev-parse failed: {e}"))?;
    if head.code != 0 {
        return Err(format!(
            "git rev-parse exit {}: {}",
            head.code,
            head.stderr.trim()
        ));
    }
    let branch = head.stdout.trim();
    if branch != expected_branch {
        return Err(format!(
            "HEAD is '{branch}', not the base branch '{expected_branch}'"
        ));
    }
    // UNPUSHED-BASE check (skeptic finding 4): branch-name-only let a weeks-stale unpushed main
    // count as "clean". Compare against the LOCAL remote-tracking ref (no network). A repo with
    // no upstream configured (rev-parse @{upstream} fails) skips this check.
    let upstream = proc::run(
        &[
            "git",
            "-C",
            &root_s,
            "rev-parse",
            "--abbrev-ref",
            &format!("{branch}@{{upstream}}"),
        ],
        None,
        Some(Duration::from_secs(60)),
    );
    if let Ok(up) = upstream {
        if up.code == 0 && !up.stdout.trim().is_empty() {
            let ahead = proc::run(
                &[
                    "git",
                    "-C",
                    &root_s,
                    "rev-list",
                    "--count",
                    &format!("{}..HEAD", up.stdout.trim()),
                ],
                None,
                Some(Duration::from_secs(60)),
            )
            .map_err(|e| format!("git rev-list failed: {e}"))?;
            if ahead.code != 0 {
                return Err(format!(
                    "git rev-list exit {}: {}",
                    ahead.code,
                    ahead.stderr.trim()
                ));
            }
            let n: u64 = ahead.stdout.trim().parse().unwrap_or(0);
            if n > 0 {
                return Err(format!(
                    "base '{branch}' has {n} commit(s) not on its upstream '{}' — push or revert \
                     them (an unpushed controller base is out-of-band; catalog #6)",
                    up.stdout.trim()
                ));
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- //
// SHA-256 (FIPS 180-4) — local implementation: the crate tree deliberately carries no crypto
// dependency, and ~50 lines beats a new supply-chain edge for one baseline hash. Verified against
// the standard test vectors below.
// --------------------------------------------------------------------------- //

fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bitlen = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

// --------------------------------------------------------------------------- //
// tests
// --------------------------------------------------------------------------- //
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn tmp_git_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "solomon_prov_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
            vec!["branch", "-M", "main"],
        ] {
            let st = Command::new("git").args(&args).current_dir(&dir).status().unwrap();
            assert!(st.success(), "git {args:?} failed in {dir:?}");
        }
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(st.success(), "git {args:?} failed in {dir:?}");
    }

    // -------- SHA-256 standard vectors --------
    #[test]
    fn sha256_test_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 bytes — crosses the two-block padding boundary.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    // -------- held_lanes: live_app OR tiers, never plain lanes --------
    #[test]
    fn held_lanes_selects_trading_adjacent_rows() {
        let rows = vec![
            json!({"name": "sover", "live_app": true}),
            json!({"name": "maki"}),
            json!({"name": "kairos", "tiers": {"non_money": "auto"}}),
            json!({"name": "asmodeus", "live_app": true}),
            json!({"live_app": true}), // no name -> skipped
        ];
        assert_eq!(held_lanes(&rows), vec!["sover", "kairos", "asmodeus"]);
    }

    // -------- controller_clean_at: clean / dirty / off-branch --------
    #[test]
    fn controller_clean_on_clean_main_is_ok() {
        let dir = tmp_git_repo("clean");
        assert_eq!(controller_clean_at(&dir, "main"), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn controller_clean_dirty_tree_is_err() {
        let dir = tmp_git_repo("dirty");
        std::fs::write(dir.join("stray.rs"), b"x").unwrap();
        let err = controller_clean_at(&dir, "main").unwrap_err();
        assert!(err.contains("dirty tree"), "{err}");
        assert!(err.contains("stray.rs"), "{err}");
        // runtime/-only dirt is EXCLUDED (gitignored in prod; the filter is belt-and-braces).
        let _ = std::fs::remove_file(dir.join("stray.rs"));
        std::fs::create_dir_all(dir.join("runtime")).unwrap();
        std::fs::write(dir.join("runtime").join("x.log"), b"x").unwrap();
        std::fs::write(dir.join(".gitignore"), b"runtime/\n").unwrap();
        git(&dir, &["add", ".gitignore"]);
        git(&dir, &["commit", "-m", "ignore runtime"]);
        assert_eq!(controller_clean_at(&dir, "main"), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------- valid_provenance_subject: the tag grammar --------
    #[test]
    fn provenance_subject_grammar_accepts_the_two_categories_only() {
        // operator + rsi family (incl. version-suffixed engine tags)
        assert!(valid_provenance_subject("operator: seed repos.json"));
        assert!(valid_provenance_subject("rsi: park ollama-cloud/glm-5.2 until 2026-07-07"));
        assert!(valid_provenance_subject("rsi-v3: supervisor diagnosis->action table"));
        assert!(valid_provenance_subject("rsi-v3.1: skeptic fixes"));
        // merge commits excepted (git writes their subjects)
        assert!(valid_provenance_subject("Merge pull request #49 from x/y"));
        // everything else is a violation
        assert!(!valid_provenance_subject("feat: expose solomon autopilot console"));
        assert!(!valid_provenance_subject("fix repos.json provider drift"));
        assert!(!valid_provenance_subject("operator:missing-space"));
        assert!(!valid_provenance_subject("operator: ")); // empty body
        assert!(!valid_provenance_subject("rsi-x: wrong suffix shape"));
        assert!(!valid_provenance_subject("rsi-v: no version digit"));
        assert!(!valid_provenance_subject("rsi-V3: uppercase"));
        assert!(!valid_provenance_subject("rsiv3: missing dash"));
        assert!(!valid_provenance_subject(""));
    }

    // -------- the audit query, enforced: watched-file commits since the convention landed --------
    // PROVENANCE.md declares `git log --oneline -- repos.json ops.json actions.json` as "the
    // check"; before this test nothing ran it (skeptic finding 4: a write-only convention).
    // Scope: commits AFTER 4521559 (the commit that introduced PROVENANCE.md) — history predating
    // the convention is not retroactively judged. Skips (passes) only when git itself cannot
    // resolve the range (e.g. a shallow clone), because an indeterminate answer is a tooling gap,
    // not a violation.
    #[test]
    fn watched_file_commits_since_convention_carry_provenance_tags() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let out = Command::new("git")
            .args([
                "log",
                "--format=%h%x09%s",
                "4521559..HEAD",
                "--",
                "repos.json",
                "ops.json",
                "actions.json",
            ])
            .current_dir(&repo_root)
            .output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skipping: git log could not resolve the convention range here");
                return;
            }
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let violations: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| {
                let subject = l.splitn(2, '\t').nth(1).unwrap_or("");
                !valid_provenance_subject(subject)
            })
            .collect();
        assert!(
            violations.is_empty(),
            "watched-file commits without a provenance tag (docs/rsi/PROVENANCE.md): {violations:?}"
        );
    }

    #[test]
    fn controller_clean_off_branch_is_err() {
        let dir = tmp_git_repo("branch");
        git(&dir, &["checkout", "-b", "rsi/iter-x"]);
        let err = controller_clean_at(&dir, "main").unwrap_err();
        assert!(err.contains("rsi/iter-x"), "{err}");
        assert!(err.contains("'main'"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------- unpushed base: clean branch name is NOT enough (skeptic finding 4) --------
    #[test]
    fn controller_clean_unpushed_base_is_err_and_pushed_is_ok() {
        let dir = tmp_git_repo("unpushed");
        // a bare "origin" + tracking main -> upstream configured, in sync -> clean
        let remote = std::env::temp_dir().join(format!(
            "solomon_prov_remote_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&remote);
        std::fs::create_dir_all(&remote).unwrap();
        let st = Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&remote)
            .status()
            .unwrap();
        assert!(st.success());
        git(&dir, &["remote", "add", "origin", &remote.to_string_lossy()]);
        git(&dir, &["push", "-u", "origin", "main"]);
        assert_eq!(controller_clean_at(&dir, "main"), Ok(()));
        // a local commit not on origin -> Err (a stale unpushed main is out-of-band, not clean)
        git(&dir, &["commit", "--allow-empty", "-m", "operator: local only"]);
        let err = controller_clean_at(&dir, "main").unwrap_err();
        assert!(err.contains("not on its upstream"), "{err}");
        assert!(err.contains("1 commit"), "{err}");
        // pushing heals it
        git(&dir, &["push", "origin", "main"]);
        assert_eq!(controller_clean_at(&dir, "main"), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&remote);
    }

    // (repos WITHOUT an upstream skip the unpushed check — every other controller_clean test in
    // this module runs on a remoteless tmp repo and stays green, which is that case's coverage.)

    // -------- check_impl: the full drift lifecycle on a tmp repo --------
    // touch (uncommitted) -> grace (no page) -> >600s (ONE page + HOLD_META in trading-adjacent
    // lanes only, deduped on re-sweep) -> commit -> cleared (holds + marker gone).
    #[test]
    fn drift_pages_once_holds_trading_lanes_and_clears_on_commit() {
        let _env = crate::notify::NOTIFY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = tmp_git_repo("drift");
        std::fs::write(repo.join("repos.json"), b"[{\"name\":\"x\"}]").unwrap();
        git(&repo, &["add", "repos.json"]);
        git(&repo, &["commit", "-m", "operator: seed repos.json"]);
        let rt = repo.join("runtime");
        std::fs::create_dir_all(&rt).unwrap();
        let rows = vec![
            json!({"name": "sover", "live_app": true}),
            json!({"name": "maki"}),
            json!({"name": "kairos", "tiers": {}}),
        ];
        let t0 = 1_750_000_000.0_f64;

        // Clean pass: baselines established, nothing paged.
        let out = check_impl(&repo, &rt, &rows, t0);
        assert!(out["actions"].as_array().unwrap().is_empty());

        // Whitespace touch, uncommitted.
        std::fs::write(repo.join("repos.json"), b"[{\"name\":\"x\"}] ").unwrap();

        // Inside the grace window: stamped, but no page and no holds.
        let out = check_impl(&repo, &rt, &rows, t0 + 60.0);
        assert!(out["actions"].as_array().unwrap().is_empty());
        assert!(!rt.join("sover").join("HOLD_META").exists());
        assert!(out["files"]["repos.json"]["first_seen_drift"].as_f64().unwrap() > 0.0);

        // Past the grace window: exactly one page action + holds in sover/kairos, NOT maki.
        let out = check_impl(&repo, &rt, &rows, t0 + 700.0);
        let acts: Vec<String> = out["actions"]
            .as_array().unwrap().iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        assert!(acts.iter().any(|a| a.contains("PAGED") && a.contains("repos.json")), "{acts:?}");
        assert!(rt.join("_config_drift_paged_repos.json").exists());
        for lane in ["sover", "kairos"] {
            let hold: Value = serde_json::from_str(
                &std::fs::read_to_string(rt.join(lane).join("HOLD_META")).unwrap(),
            ).unwrap();
            assert_eq!(hold["file"], "repos.json");
            assert_eq!(hold["reason"], "unversioned config mutation: repos.json");
            assert!(hold["expires_at"].as_f64().unwrap() > t0 + 700.0);
        }
        assert!(!rt.join("maki").join("HOLD_META").exists(), "non-trading lane never held");

        // Persisting drift on the next sweep: NO second page action (marker dedupe), holds refresh.
        let out = check_impl(&repo, &rt, &rows, t0 + 800.0);
        assert!(
            !out["actions"].as_array().unwrap().iter().any(|a| a.as_str().unwrap().contains("PAGED")),
            "a persisting drift must not re-page"
        );
        assert!(rt.join("sover").join("HOLD_META").exists());

        // Commit (provenance-tagged) -> drift clears: holds + marker deleted, action says so.
        git(&repo, &["add", "repos.json"]);
        git(&repo, &["commit", "-m", "operator: whitespace touch"]);
        let out = check_impl(&repo, &rt, &rows, t0 + 900.0);
        let acts: Vec<String> = out["actions"]
            .as_array().unwrap().iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        assert!(acts.iter().any(|a| a.contains("cleared") && a.contains("repos.json")), "{acts:?}");
        assert!(!rt.join("_config_drift_paged_repos.json").exists());
        assert!(!rt.join("sover").join("HOLD_META").exists());
        assert!(!rt.join("kairos").join("HOLD_META").exists());
        assert_eq!(out["files"]["repos.json"]["first_seen_drift"], 0.0);

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
        let _ = std::fs::remove_dir_all(&repo);
    }

    // -------- .env: pages+holds, then ADOPTS the new baseline after the hold TTL --------
    // Secrets have no commit path out of drift, so the degraded-mode fallback (requirement 7) is
    // baseline adoption after page + one full hold TTL — never a forever-hold dead end.
    #[test]
    fn env_drift_pages_holds_then_adopts_baseline() {
        let _env = crate::notify::NOTIFY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let repo = tmp_git_repo("envdrift");
        std::fs::write(repo.join(".env"), b"KEY=old").unwrap();
        let rt = repo.join("runtime");
        std::fs::create_dir_all(&rt).unwrap();
        let rows = vec![json!({"name": "sover", "live_app": true})];
        let t0 = 1_750_000_000.0_f64;

        check_impl(&repo, &rt, &rows, t0); // baseline
        std::fs::write(repo.join(".env"), b"KEY=new").unwrap();
        check_impl(&repo, &rt, &rows, t0 + 30.0); // stamps drift

        // Past grace: page + hold, baseline NOT yet adopted (revert can still clear it).
        let out = check_impl(&repo, &rt, &rows, t0 + 700.0);
        assert!(out["actions"].as_array().unwrap().iter().any(|a| a.as_str().unwrap().contains("PAGED")));
        assert!(rt.join("sover").join("HOLD_META").exists());
        assert!(rt.join("_config_drift_paged_.env").exists());

        // Past grace + hold TTL: adopted — baseline updated, artifacts cleared, honest action.
        let out = check_impl(&repo, &rt, &rows, t0 + 30.0 + 600.0 + 3600.0 + 60.0);
        assert!(
            out["actions"].as_array().unwrap().iter().any(|a| a.as_str().unwrap().contains("adopted")),
            "{out}"
        );
        assert!(!rt.join("sover").join("HOLD_META").exists());
        assert!(!rt.join("_config_drift_paged_.env").exists());
        assert_eq!(out["files"][".env"]["first_seen_drift"], 0.0);

        // And the next sweep sees no drift (the new content IS the baseline now).
        let out = check_impl(&repo, &rt, &rows, t0 + 5000.0);
        assert!(out["actions"].as_array().unwrap().is_empty());

        std::env::remove_var("SOLOMON_NOTIFY_OFF");
        let _ = std::fs::remove_dir_all(&repo);
    }

    // -------- clear_drift_artifacts leaves holds for OTHER files in place --------
    #[test]
    fn clearing_one_file_spares_other_files_holds() {
        let rt = std::env::temp_dir().join(format!("solomon_prov_spare_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(rt.join("sover")).unwrap();
        std::fs::write(
            rt.join("sover").join("HOLD_META"),
            json!({"file": "ops.json", "reason": "unversioned config mutation: ops.json"}).to_string(),
        ).unwrap();
        std::fs::write(rt.join("_config_drift_paged_repos.json"), "x").unwrap();
        clear_drift_artifacts(&rt, "repos.json");
        assert!(rt.join("sover").join("HOLD_META").exists(), "ops.json hold must survive");
        assert!(!rt.join("_config_drift_paged_repos.json").exists());
        let _ = std::fs::remove_dir_all(&rt);
    }
}
