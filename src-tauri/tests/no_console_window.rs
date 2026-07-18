//! Regression guard for the windows-headless-subprocess convention (AGENTS.md + the
//! `.claude/skills/.../windows-headless-subprocess` skill): every `std::process::Command::new(...)`
//! spawn in PRODUCTION code must arm `CREATE_NO_WINDOW` (directly via `creation_flags(...)`, or
//! indirectly through a helper that applies it — `apply_hidden`, `apply_hidden_group`,
//! `apply_spawn_flags`, `apply_visible_console`, `hidden_flags`, `run_command_bounded`,
//! `run_command_timed`, `run_command_prepared`, `run_prepared`, `proc::run`, `proc::run_win_shell`).
//! An unguarded spawn pops a console window on Windows, interrupting the operator — the exact
//! regression this test exists to catch at `cargo test` time before it ships.
//!
//! ## Scan heuristic (line-based, NOT a full Rust parser)
//!
//! 1. Walk every `.rs` file under `src-tauri/src/` with `std::fs` + `std::path`. `tests/` and
//!    `target/` are never walked.
//! 2. For each file, find every line containing the substring `Command::new(`. This catches both
//!    `std::process::Command::new(` and bare `Command::new(` after a `use std::process::Command;`.
//!    A grep over the current source confirms no `Command::new(` appears inside a `//` comment or
//!    a string literal, so a raw substring match is safe for THIS codebase; a future author who
//!    puts `Command::new(` in a doc-comment would get a false positive (fix the scanner then — do
//!    NOT silence by editing source).
//! 3. Test-mod detection (the simpler approach the task sanctions): in this repo every
//!    `#[cfg(test)] mod tests { ... }` block sits at the END of its file (audited: every file
//!    with a `Command::new` has its first `mod tests {` AFTER every production spawn). So the
//!    scanner finds the FIRST line opening a `mod tests` block (any visibility prefix: `mod tests
//!    {`, `pub mod tests {`, `pub(crate) mod tests {`), and treats every `Command::new` AT OR
//!    AFTER that line as in-test (exempt). This correctly handles (a) the usual top-level
//!    `mod tests { ... }` at file end and (b) the nested `#[cfg(test)] mod tests` inside
//!    `control/proc.rs::app_job` (line 275) — the production spawns in `proc.rs` (lines 76, 109)
//!    sit BEFORE it, and the only post-275 `Command::new` (line 287) is genuinely inside the
//!    nested test mod. A robust brace-counter was tried first but is fragile in the face of
//!    char literals like `'"'` (e.g. `supervisor.rs:3421`) that flip a naïve string-state toggle;
//!    the line-based "first `mod tests {`" rule is simpler AND correct for this codebase.
//! 4. For each PRODUCTION `Command::new` site, look at the next 25 lines and require at least one
//!    guard-marker substring (case-sensitive). The window does NOT stop early at the next
//!    `Command::new(`: the one case of two adjacent `Command::new` calls in the codebase
//!    (`improver/gates.rs::shell_command` lines 998 + 1002) are SIBLING `if cfg!(windows)` / `else`
//!    branches sharing a single guard applied later in `run_command_timed` (line 1017); stopping
//!    the window at the sibling would falsely flag the first branch. The 25-line window is
//!    generous: every helper call or `creation_flags(...)` sits within ~12 lines of its
//!    `Command::new`. A site that builds a `Command` in one function and guards it in another
//!    (`shell_command` → `run_command_timed`) is caught because the guard sits <25 lines forward.
//! 5. Two exemptions, both documented here and applied by the scanner:
//!    - **Non-console program literals** `/bin/sh`, `/bin/bash`, `/bin/zsh`: Unix-only shells that
//!      do not exist on Windows (the branch is runtime-dead on the Windows target), so there is no
//!      console window to hide. Matches the `#[cfg(not(windows))]` / `else` branches of
//!      `shell_command` in `improver/gates.rs` and `improver/freshness.rs`.
//!    - **Intentional GUI-handler spawns in `api.rs::open_in_browser`** (lines 1096 and 1109):
//!      `rundll32 url.dll,FileProtocolHandler <url>` on Windows and `open`/`xdg-open` elsewhere are
//!      the OS default-protocol handlers (mirrors Python `webbrowser.open`); they hand the URL to a
//!      GUI handler and do NOT create a console window. Allowlisted by `(file, line)` because the
//!      program arg is a variable (`&argv[0]` / `opener`), not a string literal.
//!
//! ## Known limitations (documented; do not widen without re-auditing every site)
//!
//! - Line-based: a `Command::new(` split across two lines would be missed. None exist today.
//! - Test-mod detection assumes `mod tests {` is at file end (true for every audited file). If a
//!   future file puts production spawns AFTER a `mod tests {` block, they'd be wrongly exempted.
//!   Fix by switching `test_mod_start_line` back to a brace-counted span (and handle char-literal
//!   `'"'` / `'{'` / `'}'` in the brace counter — the gotcha that forced this simpler approach).
//! - The 25-line forward window is a heuristic; a future author who moves the guard >25 lines
//!   away from the spawn would get a false positive. Fix by widening the window OR by adding the
//!   new helper name to `GUARD_MARKERS` — never by editing the spawn site to silence the guard.
//! - The window does not stop at the next `Command::new(` (see step 4). A future file with two
//!   sequential spawns where only the SECOND is guarded and the first is not, both within 25
//!   lines, would let the first borrow the second's guard (false negative). None exist today; if
//!   one is added, reintroduce the early-stop BUT skip sibling `if/else` branches — or just guard
//!   each spawn explicitly.
//! - The `(file, line)` allowlist for `open_in_browser` is brittle to line edits in `api.rs`.
//!   If `api.rs` is refactored, update `BROWSER_OPEN_SITES` or switch the spawn to a helper.
//! - `#[cfg(windows)]` vs `#[cfg(not(windows))]` is NOT tracked: a Windows-only spawn in a
//!   `#[cfg(windows)]` block still requires a guard marker (correct — it WILL pop a console on
//!   Windows). A non-Windows-only spawn is exempted via the program-literal allowlist above.

use std::fs;
use std::path::{Path, PathBuf};

/// Substrings that, appearing within 25 lines after a production `Command::new(`, count as a
/// valid console-hiding guard. Case-sensitive substring match on the forward window.
const GUARD_MARKERS: &[&str] = &[
    "creation_flags(",
    "configure_hidden(",
    "hidden_flags(",
    "apply_hidden(",
    "apply_hidden_group(",
    "apply_spawn_flags(",
    "apply_visible_console(",
    "run_command_bounded(",
    "run_command_timed(",
    "run_command_prepared(",
    "run_prepared(",
    "proc::run(",
    "proc::run_win_shell(",
];

/// Program-name string literals that do NOT create a console window on the Windows target (Unix
/// shells absent from Windows; the branch is runtime-dead there). Exempted by exact arg match.
const NON_CONSOLE_PROGRAM_LITERALS: &[&str] = &["\"/bin/sh\"", "\"/bin/bash\"", "\"/bin/zsh\""];

/// Explicit `(relative_path, 1-based line)` allowlist for intentional GUI-handler spawns whose
/// program arg is a variable (not a literal), so the program-literal exemption can't catch them.
/// `api.rs::open_in_browser` — `rundll32 url.dll,FileProtocolHandler` / `open` / `xdg-open` — are OS
/// default-protocol handlers that do not create a console window (mirrors Python `webbrowser.open`).
const BROWSER_OPEN_SITES: &[(&str, u32)] = &[
    ("api.rs", 1096),
    ("api.rs", 1109),
];

/// How many lines forward from a `Command::new(` to look for a guard marker. Generous for the
/// current codebase (the farthest guard sits ~12 lines away). The window does NOT stop early at
/// the next `Command::new(` — see the module doc (step 4) for the sibling-branch rationale.
const GUARD_WINDOW: usize = 25;

/// One unguarded production spawn found by the scanner.
#[derive(Debug, Clone)]
struct Violation {
    file: String,
    line: u32,
}

/// The root walked by the scanner: `src-tauri/src/` relative to the integration-test's CARGO_MANIFEST_DIR
/// (i.e. the `src-tauri/` dir). Integration tests compile with their CARGO_MANIFEST_DIR set to the
/// package manifest dir (`src-tauri/`), so `src/` is the source root.
fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect every `.rs` file under `root`, skipping nothing (the caller passes `src/`,
/// which already excludes `tests/` and `target/`). Sorted for deterministic output.
fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// Strip everything from the first `//` onward so a `Command::new(` in a `//`/`///`/`//!`
/// line-comment or doc-comment is not mistaken for a real call site. Returns the code portion
/// of the line (empty if the whole line is a comment). A `Command::new(` inside a block-comment
/// `/* ... */` is NOT handled — none exist in the current source; if one is added, the scanner
/// will false-positive (fix the scanner then). `//` inside a string literal on a line that ALSO
/// contains `Command::new(` does not occur in this codebase, so a plain `find("//")` is safe here.
fn strip_line_comment(line: &str) -> &str {
    if let Some(idx) = line.find("//") {
        &line[..idx]
    } else {
        line
    }
}

/// Return the 0-based index of the FIRST line that opens a `mod tests` block in the file, or
/// `usize::MAX` if there is none. Recognized openers (trimmed): `mod tests {`, `pub mod tests {`,
/// `pub(crate) mod tests {` (any spacing before the `{`). Per the doc comment, every production
/// `Command::new` in this codebase sits BEFORE the first `mod tests {`, so "line >= start" ⇒ test.
/// `mod tests;` (separate-file) does NOT occur in this repo and is NOT recognized (no inline block).
fn test_mod_start_line(lines: &[&str]) -> usize {
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let rest = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        if rest.starts_with("mod tests") {
            // Confirm a `{` follows on the same line (inline block, not `mod tests;`).
            if rest.contains('{') {
                return i;
            }
        }
    }
    usize::MAX
}

/// True if a 1-based `line` is at or after the first `mod tests {` opener (i.e. inside test code
/// under the "test mods live at file end" assumption). `start == usize::MAX` ⇒ no test mod ⇒ never.
fn in_test_mod(line_1based: u32, start: usize) -> bool {
    if start == usize::MAX {
        return false;
    }
    (line_1based as usize).saturating_sub(1) >= start
}

/// Extract the program-name argument from a `Command::new(...)` call site line, for matching
/// against `NON_CONSOLE_PROGRAM_LITERALS`. Returns the trimmed substring between the first
/// `Command::new(` and its matching close-paren on the SAME line. If the arg spans multiple lines
/// or is not a string literal, returns `None` (no literal exemption applies).
fn command_new_arg_literal(line: &str) -> Option<String> {
    let key = "Command::new(";
    let start = line.find(key)?;
    let after = &line[start + key.len()..];
    // Find the matching close paren on this line (no nested parens in a simple arg case).
    let mut depth = 1i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut end = 0;
    for (i, c) in after.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_str => escaped = true,
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    let arg = after[..end].trim();
    Some(arg.to_string())
}

/// Relative path (with forward slashes) from the source root to `abs`, e.g. `api.rs` or
/// `improver/oneshot.rs`. Falls back to the file_name if it isn't under `src/`.
fn rel_path(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| abs.file_name().unwrap_or_default().to_string_lossy().into_owned())
}

/// Scan every `.rs` file under `src/` and return every production `Command::new` site that lacks a
/// guard within its forward window. Also returns (production_total, guarded_total) for reporting.
fn scan(root: &Path) -> (Vec<Violation>, usize, usize) {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    let mut violations = Vec::new();
    let mut prod_total = 0usize;
    let mut guarded_total = 0usize;
    for f in &files {
        let Ok(src) = fs::read_to_string(f) else { continue };
        let lines: Vec<&str> = src.lines().collect();
        let tests_start = test_mod_start_line(&lines);
        let rel = rel_path(f, root);

        // (line_1based) of every `Command::new(` call site in this file (after stripping comments).
        let mut sites: Vec<u32> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if strip_line_comment(line).contains("Command::new(") {
                sites.push((i + 1) as u32);
            }
        }

        for (_idx, &line1) in sites.iter().enumerate() {
            if in_test_mod(line1, tests_start) {
                continue; // test code — exempt
            }
            prod_total += 1;
            let line0 = (line1 as usize) - 1;
            let raw = lines[line0];
            let arg = command_new_arg_literal(strip_line_comment(raw));

            // Exemption 1: non-console program literal (Unix shell, dead on Windows).
            let is_non_console_literal = arg
                .as_deref()
                .map(|a| NON_CONSOLE_PROGRAM_LITERALS.iter().any(|lit| *lit == a))
                .unwrap_or(false);

            // Exemption 2: explicit (file, line) allowlist for intentional browser-open spawns.
            let is_browser_open = BROWSER_OPEN_SITES
                .iter()
                .any(|(p, l)| *p == &rel && *l == line1);

            if is_non_console_literal || is_browser_open {
                guarded_total += 1;
                continue;
            }

            // Forward window: GUARD_WINDOW lines after the call site. Does NOT stop early at the next
            // `Command::new(` — see doc step 4 (sibling if/else branches in `shell_command` share a
            // single later guard; stopping at the sibling would falsely flag the first branch).
            let window_end = (line0 + 1 + GUARD_WINDOW).min(lines.len());
            let mut window = String::new();
            for li in (line0 + 1)..window_end {
                window.push_str(strip_line_comment(lines[li]));
                window.push('\n');
            }

            let guarded = GUARD_MARKERS.iter().any(|m| window.contains(m));
            if guarded {
                guarded_total += 1;
            } else {
                violations.push(Violation {
                    file: rel.clone(),
                    line: line1,
                });
            }
        }
    }
    (violations, prod_total, guarded_total)
}

#[test]
fn no_unguarded_command_spawns_in_production() {
    let root = source_root();
    assert!(
        root.is_dir(),
        "source root not found: {} (expected src-tauri/src/ under CARGO_MANIFEST_DIR)",
        root.display()
    );
    let (violations, prod_total, guarded_total) = scan(&root);
    if !violations.is_empty() {
        let mut msg = String::new();
        msg.push_str(&format!(
            "found {} unguarded `Command::new` spawn(s) in production code \
             (console window would pop on Windows). Each site must arm CREATE_NO_WINDOW, either \
             directly (`creation_flags(proc::CREATE_NO_WINDOW)`) or via a helper that applies it \
             (`apply_hidden`, `apply_hidden_group`, `apply_spawn_flags`, `apply_visible_console`, \
             `hidden_flags`, `run_command_bounded`, `run_command_timed`, `run_command_prepared`, \
             `run_prepared`, `proc::run`, `proc::run_win_shell`). Add the guard within 25 lines \
             of the spawn, OR route the spawn through a helper — do NOT silence this test by \
             editing the spawn site.\n\n",
            violations.len()
        ));
        for v in &violations {
            msg.push_str(&format!("  {}:{}\n", v.file, v.line));
        }
        msg.push_str(&format!(
            "\n(scanned {} production `Command::new` sites; {} guarded, {} unguarded)",
            prod_total,
            guarded_total,
            violations.len()
        ));
        panic!("{msg}");
    }
    // Always report the counts so a `--nocapture` run shows the audit surface.
    eprintln!(
        "no_console_window: scanned {} production `Command::new` sites; all {} guarded (0 violations)",
        prod_total, guarded_total
    );
}

#[test]
fn guard_detects_planted_violation() {
    // Plant a temp `.rs` file with a bare `Command::new("cmd")` NOT in a test mod and confirm the
    // scanner flags it. This proves the guard actually enforces — mirroring the windows-headless
    // skill's "plant a bare spawn and confirm it fails" rule. We scan a temp root containing only
    // the planted file (not the real `src/`) to keep the assertion focused and fast.
    let tmp = std::env::temp_dir().join(format!(
        "solomon_no_console_window_plant_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&tmp).expect("create temp scan root");
    let planted = tmp.join("planted.rs");
    fs::write(
        &planted,
        "use std::process::Command;\n\
         fn main() {\n\
         let mut c = Command::new(\"cmd\");\n\
         c.arg(\"/c\").arg(\"echo\").arg(\"hi\");\n\
         let _ = c.spawn();\n\
         }\n",
    )
    .expect("write planted violation");

    let (violations, prod_total, _guarded) = scan(&tmp);
    assert_eq!(
        prod_total, 1,
        "planted file should have exactly 1 production `Command::new` site, got {prod_total}"
    );
    assert_eq!(
        violations.len(),
        1,
        "scanner must flag the planted bare `Command::new` (got {} violations: {:?})",
        violations.len(),
        violations
    );
    assert_eq!(violations[0].file, "planted.rs");
    assert_eq!(violations[0].line, 3);

    // Negative control: the same file WITH a guard marker within the window must NOT be flagged.
    let guarded_path = tmp.join("guarded.rs");
    fs::write(
        &guarded_path,
        "use std::process::Command;\n\
         fn main() {\n\
         let mut c = Command::new(\"cmd\");\n\
         c.arg(\"/c\").arg(\"echo\").arg(\"hi\");\n\
         c.creation_flags(0x0800_0000);\n\
         let _ = c.spawn();\n\
         }\n",
    )
    .expect("write guarded control");
    let (v2, prod2, _g2) = scan(&tmp);
    assert_eq!(
        prod2, 2,
        "temp root should now have 2 production sites (planted + guarded), got {prod2}"
    );
    let only_planted: Vec<_> = v2.iter().filter(|v| v.file == "planted.rs").collect();
    assert_eq!(
        only_planted.len(),
        1,
        "planted (unguarded) file should still be the only violation, got {v2:?}"
    );

    // Negative control 2: a `Command::new` INSIDE a `#[cfg(test)] mod tests` must NOT be flagged.
    let testmod_path = tmp.join("testmod.rs");
    fs::write(
        &testmod_path,
        "use std::process::Command;\n\
         fn prod() {}\n\
         #[cfg(test)]\n\
         mod tests {\n\
         use super::*;\n\
         #[test]\n\
         fn t() {\n\
         let _ = Command::new(\"cmd\").spawn();\n\
         }\n\
         }\n",
    )
    .expect("write testmod control");
    let (v3, prod3, _g3) = scan(&tmp);
    assert_eq!(
        prod3, 2,
        "testmod site must be excluded from production count, got {prod3}"
    );
    let testmod_flagged: Vec<_> = v3.iter().filter(|v| v.file == "testmod.rs").collect();
    assert!(
        testmod_flagged.is_empty(),
        "test-mod `Command::new` must NOT be flagged, got {testmod_flagged:?}"
    );

    let _ = fs::remove_dir_all(&tmp);
}