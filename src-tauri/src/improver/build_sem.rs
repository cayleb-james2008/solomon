//! Cross-process counting semaphore bounding how many lanes run their cargo gate at once.
//!
//! Each lane is a SEPARATE OS process (`Solomon.exe run-improver <name>`), so an in-process
//! `Semaphore` cannot bound the AGGREGATE cargo load — this coordinates through the filesystem.
//! N slot files live under `runtime/build-slots/slot-<i>` (sibling of the per-lane `runtime/<name>/
//! lock`). A slot is claimed with the SAME primitive `control::locks` uses for the supervisor lock:
//! an atomic O_EXCL `create_new`, `<pid>\n<token>` content, `pid_alive()` liveness, and stale-holder
//! takeover — so a slot pinned by a CRASHED lane is reclaimable. Acquire WAITS while every slot is
//! held by a LIVE builder (a real build is running and WILL free a slot when it finishes); it only
//! proceeds oversubscribed after T_MAX (a safety valve == one gate's own max duration, so a lane can
//! never deadlock behind a stuck cohort), which is why it still BINDS concurrency under the sustained
//! multi-lane load that actually pegs the box. Release is RAII: `BuildSlot::drop` removes the file,
//! so every gate exit path (the revert `return`s, a panic, or the normal fall-through) frees the
//! slot with no explicit call.

use crate::control::locks;
use crate::improver::ctx::{self, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Concurrent cargo gates allowed fleet-wide. With the companion CARGO_BUILD_JOBS=3 cap, 3 × 3 = 9
/// of 12 cores at peak — leaving headroom so the GUI + live apps stay responsive (the box is never
/// wedged), while still letting three lanes make gate progress at once.
const BUILD_SLOTS: usize = 3;
/// Base poll interval while all slots are held by live builders.
const POLL_BASE: Duration = Duration::from_millis(500);
/// Max extra per-poll jitter, de-syncing lanes that started their gate on the same watchdog tick so
/// they don't stampede the same freed slot.
const POLL_JITTER_MS: u64 = 250;

/// RAII handle to one held build slot. Dropping it releases the slot (token-guarded file removal, so
/// a stale-takeover by another lane is never clobbered). `slot == None` means we FELL BACK (proceeded
/// oversubscribed after the safety valve) — drop is then a no-op.
pub struct BuildSlot {
    slot: Option<PathBuf>,
    token: String,
    acquired: bool,
}

impl BuildSlot {
    /// True when a real slot is held; false when proceeding oversubscribed after the safety valve.
    pub fn acquired(&self) -> bool {
        self.acquired
    }
}

impl Drop for BuildSlot {
    fn drop(&mut self) {
        let path = match &self.slot {
            Some(p) => p,
            None => return, // fell back — nothing to release
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // Release ONLY if WE still own it (token match) — a stale-takeover by another lane must not
        // be clobbered. Mirrors release_supervisor_lock's own-token guard.
        if locks::read_lock_dir(dir, fname).1.as_deref() == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// `<control>/runtime/build-slots` — the same runtime tree the per-lane locks live under.
fn slots_dir(c: &Ctx) -> PathBuf {
    c.control.join("runtime").join("build-slots")
}

/// Cheap per-poll jitter without a rand dep: low bits of the wall clock (varies every call, unlike a
/// freshly-created Instant whose elapsed() is ~0ns).
fn jitter_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % (POLL_JITTER_MS + 1)
}

/// Try to claim slot file `path` atomically. `Some(token)` on success. A slot held by a LIVE pid ->
/// `None` (busy). A slot held by a DEAD pid, or empty/corrupt, is taken over.
fn try_claim(path: &Path) -> Option<String> {
    let token = format!("bs-{}", locks::rand_hex32_pub());
    let content = format!("{}\n{}", std::process::id(), token);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            if f.write_all(content.as_bytes()).is_ok() {
                return Some(token);
            }
            drop(f);
            let _ = std::fs::remove_file(path); // never leave a 0-byte slot pinned forever
            return None;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => { /* fall through to takeover */ }
        Err(_) => return None, // transient ACL/IO -> treat as busy this round
    }
    // Slot exists. Is its holder alive?
    let dir = path.parent().unwrap_or(Path::new("."));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let (pid, _) = locks::read_lock_dir(dir, fname);
    if pid != 0 && locks::pid_alive(pid) {
        return None; // a live lane holds it
    }
    // Dead/empty holder — take over via tmp+rename, then confirm we won (a racer's rename lost).
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        drop(f);
        std::fs::rename(&tmp, path)
    })()
    .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    std::thread::sleep(Duration::from_millis(100)); // let a racer's rename settle, then re-read
    if locks::read_lock_dir(dir, fname).1.as_deref() == Some(token.as_str()) {
        Some(token)
    } else {
        None
    }
}

/// Acquire one of `BUILD_SLOTS` cross-process build slots. WAITS while every slot is held by a LIVE
/// builder (dead holders are reclaimed on each pass), so at most `BUILD_SLOTS` lanes build
/// concurrently under sustained load. Proceeds oversubscribed only after T_MAX (== one gate's own max
/// duration, `GATE_TIMEOUT`) so a pathological all-stuck cohort can never deadlock a lane. RAII: drop
/// releases. If the slots dir can't be created, proceeds without a slot (never gates the gate).
pub fn acquire_build_slot(c: &mut Ctx) -> BuildSlot {
    let dir = slots_dir(c);
    if std::fs::create_dir_all(&dir).is_err() {
        c.log("build-semaphore: runtime/build-slots unavailable — proceeding without a slot");
        return BuildSlot {
            slot: None,
            token: String::new(),
            acquired: false,
        };
    }
    let t_max = Duration::from_secs(ctx::GATE_TIMEOUT.max(0) as u64);
    let deadline = Instant::now() + t_max;
    let mut waited = false;
    loop {
        for i in 0..BUILD_SLOTS {
            let path = dir.join(format!("slot-{i}"));
            if let Some(token) = try_claim(&path) {
                if waited {
                    c.log("build-semaphore: slot acquired after waiting for a busy cohort");
                }
                return BuildSlot {
                    slot: Some(path),
                    token,
                    acquired: true,
                };
            }
        }
        if Instant::now() >= deadline {
            c.log(&format!(
                "build-semaphore: all {BUILD_SLOTS} slots held by live builds for {}s — proceeding oversubscribed",
                t_max.as_secs()
            ));
            return BuildSlot {
                slot: None,
                token: String::new(),
                acquired: false,
            };
        }
        waited = true;
        std::thread::sleep(POLL_BASE + Duration::from_millis(jitter_ms()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "solomon_bs_{}_{}",
            std::process::id(),
            locks::rand_hex32_pub()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // Fill all N slots, then every further claim finds none free (all held by THIS live pid).
    #[test]
    fn slots_fill_then_saturate() {
        let dir = temp_dir();
        for i in 0..BUILD_SLOTS {
            assert!(
                try_claim(&dir.join(format!("slot-{i}"))).is_some(),
                "slot {i} claimable while free"
            );
        }
        for i in 0..BUILD_SLOTS {
            assert!(
                try_claim(&dir.join(format!("slot-{i}"))).is_none(),
                "slot {i} busy (held by this live pid)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A slot pinned by a DEAD pid is reclaimable (crash-safety — no leak).
    #[test]
    fn dead_holder_reclaimed() {
        let dir = temp_dir();
        let slot = dir.join("slot-0");
        std::fs::write(&slot, "2147483646\noldtok").unwrap(); // never-alive pid
        let tok = try_claim(&slot).expect("dead holder's slot taken over");
        assert_eq!(
            locks::read_lock_dir(&dir, "slot-0").1.as_deref(),
            Some(tok.as_str()),
            "slot now carries our token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Dropping a held BuildSlot releases the file; a fallback (None) drop is a no-op.
    #[test]
    fn drop_releases_only_when_owned() {
        let dir = temp_dir();
        let slot = dir.join("slot-0");
        let token = try_claim(&slot).unwrap();
        {
            let _g = BuildSlot {
                slot: Some(slot.clone()),
                token,
                acquired: true,
            };
            assert!(slot.exists());
        } // drop here
        assert!(!slot.exists(), "own-token drop removes the slot file");

        // Re-claim, then simulate another lane's stale-takeover (different token) before our drop.
        let _token2 = try_claim(&slot).unwrap();
        std::fs::write(&slot, "999\nsomeone-elses-token").unwrap();
        {
            let _g = BuildSlot {
                slot: Some(slot.clone()),
                token: "our-old-token".to_string(),
                acquired: true,
            };
        } // drop must NOT delete a slot another lane now owns
        assert!(slot.exists(), "token-guarded drop never clobbers a takeover");

        // A fallback guard drops without touching anything.
        let _fb = BuildSlot {
            slot: None,
            token: String::new(),
            acquired: false,
        };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
