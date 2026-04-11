//! `insh doctor` — full parity with upstream's `is doctor`.
//!
//! Three check suites, each returning 0 on success and 1 on failure:
//!   1. Legacy config scan — detects old `~/.inshellisense/init/...`
//!      references in shell rc files (users migrating from upstream).
//!   2. Shell config existence — every supported shell should have a
//!      generated init file under `~/.insh-rs/init/<shell>/`.
//!   3. Shell plugin check — the user's rc file should source our init
//!      file as its LAST non-whitespace line.
//!
//! Exit code is the sum of failing check suites (0/1/2/3), matching
//! upstream's `process.exit(errors)` semantics in ui-doctor.ts.
//!
//! Visual format: `✓` (green) for passes, `•` (red) for failures,
//! byte-matching upstream's output so users migrating from inshellisense
//! see identical diagnostics.

use crate::paths;
use crate::resources::ALL_SHELLS;
use crate::shell::Shell;
use anyhow::Result;
use std::fs;

// ANSI color helpers — keep in-module to avoid adding chalk/ansi_term deps.
const GREEN_CHECK: &str = "\x1b[32m✓\x1b[0m";
const RED_BULLET: &str = "\x1b[31m•\x1b[0m";
const RED_DASH: &str = "\x1b[31m-\x1b[0m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

pub fn run() -> Result<()> {
    let mut errors = 0;
    errors += check_legacy_configs();
    errors += check_shell_plugins();
    errors += check_shell_configs();

    // Informational suffix
    println!();
    print_environment_summary();

    if errors > 0 {
        std::process::exit(errors);
    }
    Ok(())
}

/// Suite 1 — scan shell rc files for old `~/.inshellisense/...` references
/// that should be removed when migrating to insh-rs.
fn check_legacy_configs() -> i32 {
    let shells_with_legacy = shells_with_legacy_config();
    if !shells_with_legacy.is_empty() {
        eprintln!("{RED_BULLET}{BOLD} detected legacy inshellisense configurations{RESET}");
        eprintln!("  the following shells have legacy upstream configurations:");
        for s in &shells_with_legacy {
            eprintln!("  {RED_DASH} {}", s.as_str());
        }
        eprintln!(
            "{YELLOW}  remove any ~/.inshellisense/ references from your shell profile and re-add them using `insh init --install-rc`{RESET}"
        );
        return 1;
    }
    println!("{GREEN_CHECK} no legacy configurations found");
    0
}

/// Suite 2 — check that every supported shell has a generated init file
/// under `~/.insh-rs/init/<shell>/init.<ext>`.
fn check_shell_configs() -> i32 {
    let shells_without = shells_without_init_file();
    if !shells_without.is_empty() {
        eprintln!(
            "{RED_BULLET} the following shells do not have init files generated:"
        );
        for s in &shells_without {
            eprintln!("  {RED_DASH} {}", s.as_str());
        }
        eprintln!(
            "{YELLOW}  run \x1b[4m\x1b[36minsh reinit{RESET}{YELLOW} to regenerate{RESET}"
        );
        return 1;
    }
    println!("{GREEN_CHECK} all shells have init files");
    0
}

/// Suite 3 — for each shell whose rc file exists, check that our source
/// directive (or the older upstream's) is (a) present, (b) the last
/// non-whitespace line.
fn check_shell_plugins() -> i32 {
    let (without, bad) = shell_plugin_status();
    let mut failed = 0;

    if without.is_empty() {
        println!("{GREEN_CHECK} all shells have plugins installed");
    } else {
        eprintln!(
            "{RED_BULLET} the following shells do not have the insh-rs plugin installed:"
        );
        for s in &without {
            eprintln!("  {RED_DASH} {}", s.as_str());
        }
        eprintln!(
            "{YELLOW}  run \x1b[4m\x1b[36minsh init <shell> --install-rc{RESET}{YELLOW} or ignore if you prefer manual startup{RESET}"
        );
        failed = 1;
    }

    if bad.is_empty() {
        println!("{GREEN_CHECK} all shells have correct plugins");
    } else {
        eprintln!(
            "{RED_BULLET} the following shells have plugins installed but not as the last line of the rc file:"
        );
        for s in &bad {
            eprintln!("  {RED_DASH} {}", s.as_str());
        }
        eprintln!(
            "{YELLOW}  the insh-rs source line must be the last non-whitespace line in the rc file — upstream inshellisense has the same requirement{RESET}"
        );
        failed = 1;
    }

    failed
}

fn print_environment_summary() {
    println!("{BOLD}environment:{RESET}");
    println!("  version       {}", env!("CARGO_PKG_VERSION"));
    println!("  HOME          {:?}", paths::home());
    if let Some(root) = paths::resource_root() {
        println!("  resource root {} ({})", root.display(), if root.exists() { "present" } else { "absent" });
    }
    println!(
        "  session       {}",
        if crate::env::session_active() {
            "live"
        } else {
            "not in a wrapped session"
        }
    );
    println!("  js runtime    none (pure Rust)");
}

// ---------- pure logic ----------

fn shells_with_legacy_config() -> Vec<Shell> {
    ALL_SHELLS
        .iter()
        .copied()
        .filter(|&s| rc_file_contains(s, "~/.inshellisense") || rc_file_contains(s, "inshellisense/init"))
        .filter(|&s| !rc_file_contains(s, "insh-rs")) // upstream users aren't legacy from our POV
        .collect()
}

fn shells_without_init_file() -> Vec<Shell> {
    ALL_SHELLS
        .iter()
        .copied()
        .filter(|&s| paths::init_file(s).map(|p| !p.exists()).unwrap_or(true))
        .collect()
}

fn shell_plugin_status() -> (Vec<Shell>, Vec<Shell>) {
    let mut without = Vec::new();
    let mut bad = Vec::new();
    for &shell in ALL_SHELLS {
        let Some(rc) = paths::shell_rc_file(shell) else {
            // Shells whose rc file path is dynamic (pwsh/powershell/nu)
            // can't be checked statically without spawning the shell.
            // Skip for now.
            continue;
        };
        if !rc.exists() {
            // No rc file at all → treat as "not installed".
            without.push(shell);
            continue;
        }
        let Ok(contents) = fs::read_to_string(&rc) else {
            continue;
        };
        let marker = insh_rs_marker();
        if !contents.contains(marker) {
            without.push(shell);
            continue;
        }
        if !is_last_non_whitespace(&contents, marker) {
            bad.push(shell);
        }
    }
    (without, bad)
}

fn rc_file_contains(shell: Shell, needle: &str) -> bool {
    let Some(path) = paths::shell_rc_file(shell) else {
        return false;
    };
    fs::read_to_string(&path)
        .map(|c| c.contains(needle))
        .unwrap_or(false)
}

fn insh_rs_marker() -> &'static str {
    // Whatever string is unique to our install snippet. Matches the
    // MARKER constant in src/shell_init.rs.
    "# >>> insh-rs init >>>"
}

fn is_last_non_whitespace(contents: &str, marker: &str) -> bool {
    let Some(idx) = contents.rfind(marker) else {
        return false;
    };
    // Find the end of the block (the closing MARKER_END).
    let after = &contents[idx + marker.len()..];
    let end_marker = "# <<< insh-rs init <<<";
    let Some(end_rel) = after.find(end_marker) else {
        // Open marker without close — treat as corrupt
        return false;
    };
    let tail = &after[end_rel + end_marker.len()..];
    tail.chars().all(|c| c.is_whitespace())
}

// Keep the old doctor exposed under a different name for backwards compat
// of any internal callers. `run()` is the new canonical entry.
#[allow(dead_code)]
pub fn legacy_info_run() -> Result<()> {
    run()
}

// Also expose for the commands module to use instead of its own stub.
// (src/commands/doctor.rs now delegates here.)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_line_check_positive() {
        let s = "foo\nbar\n# >>> insh-rs init >>>\nline\n# <<< insh-rs init <<<\n";
        assert!(is_last_non_whitespace(s, "# >>> insh-rs init >>>"));
    }

    #[test]
    fn last_line_check_negative() {
        let s = "foo\n# >>> insh-rs init >>>\nline\n# <<< insh-rs init <<<\nalias ll=ls\n";
        assert!(!is_last_non_whitespace(s, "# >>> insh-rs init >>>"));
    }

    #[test]
    fn last_line_check_missing_end() {
        let s = "# >>> insh-rs init >>>\n(no close)\n";
        assert!(!is_last_non_whitespace(s, "# >>> insh-rs init >>>"));
    }
}
