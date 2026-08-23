//! `is doctor` — full parity with upstream's `is doctor`.
//!
//! Five check suites, each returning 0 on success and 1 on failure:
//!   1. Legacy config scan — detects old `~/.inshellisense/init/...`
//!      references in shell rc files (users migrating from upstream),
//!      ignoring the block we installed ourselves and commented-out lines.
//!   2. Shell config existence — every supported shell should have a
//!      generated init file under `~/.inshellisense/init/<shell>/`.
//!   3. Shell plugin check — the user's rc file should source our init
//!      file (as a live, non-comment line) or contain the installed wrapper
//!      block. Unreadable rc files are reported, never skipped.
//!   4. Runtime resources — the `~/.inshellisense/` tree must be complete
//!      and stamped by this build.
//!   5. Config files — parse errors and unknown keys.
//!
//! Exit code is the sum of failing check suites, matching upstream's
//! `process.exit(errors)` semantics in ui-doctor.ts.
//!
//! Visual format: `✓` (green) for passes, `•` (red) for failures,
//! byte-matching upstream's output so users migrating from inshellisense
//! see identical diagnostics.

use crate::paths;
use crate::resources::ALL_SHELLS;
use crate::shell::Shell;
use anyhow::Result;
use std::fs;
use std::io::IsTerminal;
use std::sync::OnceLock;

// ANSI color helpers — keep in-module to avoid adding chalk/ansi_term deps.

/// The doctor palette, resolved once per process from [`colors_enabled`].
///
/// Every printing function binds these as locals under their historical
/// names (`GREEN_CHECK`, …) so report lines stay byte-identical on a real
/// terminal while captured runs get plain text.
#[derive(Clone, Copy)]
struct Palette {
    green_check: &'static str,
    red_bullet: &'static str,
    red_dash: &'static str,
    yellow: &'static str,
    reset: &'static str,
    bold: &'static str,
    /// Emphasis opener for a command name inside a hint (underline + cyan).
    emph: &'static str,
}

const COLOR_PALETTE: Palette = Palette {
    green_check: "\x1b[32m✓\x1b[0m",
    red_bullet: "\x1b[31m•\x1b[0m",
    red_dash: "\x1b[31m-\x1b[0m",
    yellow: "\x1b[33m",
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    emph: "\x1b[4m\x1b[36m",
};

const PLAIN_PALETTE: Palette = Palette {
    green_check: "✓",
    red_bullet: "•",
    red_dash: "-",
    yellow: "",
    reset: "",
    bold: "",
    emph: "",
};

/// Pure decision behind [`colors_enabled`], kept testable without touching
/// process state.
fn resolve_color(no_color: Option<&std::ffi::OsStr>, tty: bool) -> bool {
    !matches!(no_color, Some(value) if !value.is_empty()) && tty
}

/// Diagnostics may carry ANSI color only when `NO_COLOR` is unset or empty
/// (the no-color.org convention: any non-empty value disables) and both
/// output streams are terminals — doctor writes to both, and one redirected
/// stream is enough to sprinkle escape bytes through a captured report.
fn colors_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        resolve_color(
            std::env::var_os("NO_COLOR").as_deref(),
            std::io::stdout().is_terminal() && std::io::stderr().is_terminal(),
        )
    })
}

fn palette() -> Palette {
    if colors_enabled() {
        COLOR_PALETTE
    } else {
        PLAIN_PALETTE
    }
}

pub fn run() -> Result<()> {
    let mut errors = 0;
    errors += check_legacy_configs();
    errors += check_shell_plugins();
    errors += check_shell_configs();
    errors += check_runtime_resources();
    errors += check_config_file();

    // Informational suffix
    println!();
    print_environment_summary();

    if errors > 0 {
        std::process::exit(errors);
    }
    Ok(())
}

/// Suite 4 — the `~/.inshellisense/` tree must be complete and current.
/// A hand-deleted integration script, a truncated or hand-edited one, or a
/// tree written by a different build of the same version otherwise goes
/// unreported.
fn check_runtime_resources() -> i32 {
    let Palette {
        green_check,
        red_bullet,
        red_dash,
        yellow,
        reset,
        emph,
        ..
    } = palette();
    let missing = crate::resources::missing_files();
    if !missing.is_empty() {
        eprintln!("{red_bullet} runtime resources are missing:");
        for path in &missing {
            eprintln!("  {red_dash} {}", path.display());
        }
        eprintln!("{yellow}  run {emph}is reinit{reset}{yellow} to restore them{reset}");
        return 1;
    }
    let mismatched = crate::resources::mismatched_files();
    if !mismatched.is_empty() {
        // A stamp can vouch for bytes that are no longer on disk (a crash
        // mid-repair, ENOSPC, or manual editing). The version file alone
        // cannot see this; only comparing contents does.
        eprintln!("{red_bullet} runtime resource contents do not match this build:");
        for path in &mismatched {
            eprintln!("  {red_dash} {}", path.display());
        }
        eprintln!("{yellow}  run {emph}is reinit{reset}{yellow} to restore them{reset}");
        return 1;
    }
    if !crate::resources::tree_is_current() {
        eprintln!("{red_bullet} runtime resources are stale (written by a different build)");
        eprintln!("{yellow}  run {emph}is reinit{reset}{yellow} to refresh them{reset}");
        return 1;
    }
    println!("{green_check} runtime resources present and current");
    0
}

/// Suite 5 — a config file that fails to parse, or names keys we ignore,
/// silently changes nothing at runtime. Surface it here.
fn check_config_file() -> i32 {
    let Palette {
        green_check,
        red_bullet,
        red_dash,
        ..
    } = palette();
    let problems = crate::config::diagnose();
    if problems.is_empty() {
        println!("{green_check} config files valid");
        return 0;
    }
    eprintln!("{red_bullet} configuration problems:");
    for problem in &problems {
        eprintln!("  {red_dash} {problem}");
    }
    1
}

/// Suite 1 — scan shell rc files for old `~/.inshellisense/...` references
/// that should be removed when migrating to inshellisense-rs.
fn check_legacy_configs() -> i32 {
    let Palette {
        green_check,
        red_bullet,
        red_dash,
        yellow,
        reset,
        bold,
        ..
    } = palette();
    let shells_with_legacy = shells_with_legacy_config();
    if !shells_with_legacy.is_empty() {
        eprintln!("{red_bullet}{bold} detected legacy inshellisense configurations{reset}");
        eprintln!("  the following shells have legacy upstream configurations:");
        for s in &shells_with_legacy {
            eprintln!("  {red_dash} {}", s.as_str());
        }
        eprintln!(
            "{yellow}  remove any ~/.inshellisense/ references from your shell profile and re-add them using `is init --install-rc`{reset}"
        );
        return 1;
    }
    println!("{green_check} no legacy configurations found");
    0
}

/// Suite 2 — check that every supported shell has a generated init file
/// under `~/.inshellisense/init/<shell>/init.<ext>`.
fn check_shell_configs() -> i32 {
    let Palette {
        green_check,
        red_bullet,
        red_dash,
        yellow,
        reset,
        emph,
        ..
    } = palette();
    let shells_without = shells_without_init_file();
    if !shells_without.is_empty() {
        eprintln!("{red_bullet} the following shells do not have init files generated:");
        for s in &shells_without {
            eprintln!("  {red_dash} {}", s.as_str());
        }
        eprintln!("{yellow}  run {emph}is reinit{reset}{yellow} to regenerate{reset}");
        return 1;
    }
    println!("{green_check} all shells have init files");
    0
}

/// Suite 3 — for each shell whose rc file exists, check that our source
/// directive (or the older upstream's) is (a) present, (b) the last
/// non-whitespace line.
fn check_shell_plugins() -> i32 {
    let Palette {
        green_check,
        red_bullet,
        red_dash,
        yellow,
        reset,
        emph,
        ..
    } = palette();
    let (without, bad, unreadable, dynamic) = shell_plugin_status();
    let mut failed = 0;

    if !unreadable.is_empty() {
        eprintln!("{red_bullet} the following shells have rc files that cannot be read:");
        for (shell, reason) in &unreadable {
            eprintln!("  {red_dash} {} ({reason})", shell.as_str());
        }
        eprintln!("{yellow}  check the file's permissions — its plugin state is unknown{reset}");
        failed = 1;
    }
    if !dynamic.is_empty() {
        let names: Vec<&str> = dynamic.iter().map(|s| s.as_str()).collect();
        println!(
            "{yellow}!{reset} profile path is resolved at runtime, not checked: {}",
            names.join(", ")
        );
    }

    if without.is_empty() {
        println!("{green_check} all shells have plugins installed");
    } else {
        eprintln!(
            "{red_bullet} the following shells do not have the inshellisense-rs plugin installed:"
        );
        for s in &without {
            eprintln!("  {red_dash} {}", s.as_str());
        }
        eprintln!(
            "{yellow}  run {emph}is init <shell> --install-rc{reset}{yellow} or ignore if you prefer manual startup{reset}"
        );
        failed = 1;
    }

    if bad.is_empty() {
        println!("{green_check} all shells have correct plugins");
    } else {
        eprintln!(
            "{red_bullet} the following shells have plugins installed but not as the last line of the rc file:"
        );
        for s in &bad {
            eprintln!("  {red_dash} {}", s.as_str());
        }
        eprintln!(
            "{yellow}  the inshellisense-rs source line must be the last non-whitespace line in the rc file — upstream inshellisense has the same requirement{reset}"
        );
        failed = 1;
    }

    failed
}

fn print_environment_summary() {
    let Palette { bold, reset, .. } = palette();
    println!("{bold}environment:{reset}");
    println!("  version       {}", env!("CARGO_PKG_VERSION"));
    println!("  HOME          {:?}", paths::home());
    if let Some(root) = paths::resource_root() {
        println!(
            "  resource root {} ({})",
            root.display(),
            if root.exists() { "present" } else { "absent" }
        );
    }
    println!(
        "  session       {}",
        if crate::env::session_active() {
            "live"
        } else {
            "not in a wrapped session"
        }
    );
    match crate::coreutils::detect() {
        Some(cu) => println!(
            "  coreutils     {} ({} utilities)",
            cu.binary.display(),
            cu.utils.len()
        ),
        None => println!("  coreutils     not installed"),
    }
    println!("  js runtime    none (pure Rust)");
}

// ---------- pure logic ----------

/// An rc file we could not read is a diagnosis, not a pass. Reporting it as
/// "no problems found" is exactly the failure mode doctor exists to catch.
enum RcFile {
    Missing,
    Unreadable(std::io::Error),
    /// Raw bytes — a profile need not be valid UTF-8.
    Contents(Vec<u8>),
}

fn read_rc(shell: Shell) -> Option<(std::path::PathBuf, RcFile)> {
    let rc = paths::shell_rc_file(shell)?;
    let state = match fs::read(&rc) {
        Ok(bytes) => RcFile::Contents(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RcFile::Missing,
        Err(e) => RcFile::Unreadable(e),
    };
    Some((rc, state))
}

/// Shells whose rc file mentions upstream's paths *outside* the block we
/// installed. Previously a current install suppressed this check entirely, so
/// a profile carrying both a legacy and a current entry reported clean.
fn shells_with_legacy_config() -> Vec<Shell> {
    ALL_SHELLS
        .iter()
        .copied()
        .filter(|&shell| {
            let Some((_, RcFile::Contents(contents))) = read_rc(shell) else {
                return false;
            };
            let remainder = crate::shell_init::strip_our_entries(shell, &contents);
            remainder
                .split(|b| *b == b'\n')
                // A user's own comment mentioning the path isn't a live config.
                .filter(|line| !is_comment(line))
                .any(|line| {
                    contains(line, b"~/.inshellisense") || contains(line, b"inshellisense/init")
                })
        })
        .collect()
}

fn is_comment(line: &[u8]) -> bool {
    matches!(line.iter().find(|b| !b.is_ascii_whitespace()), Some(&b'#'))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn shells_without_init_file() -> Vec<Shell> {
    ALL_SHELLS
        .iter()
        .copied()
        .filter(|&s| paths::init_file(s).map(|p| !p.exists()).unwrap_or(true))
        .collect()
}

/// (missing plugin, plugin not last, unreadable rc + reason, dynamic profile).
type PluginStatus = (Vec<Shell>, Vec<Shell>, Vec<(Shell, String)>, Vec<Shell>);

fn shell_plugin_status() -> PluginStatus {
    let mut without = Vec::new();
    let mut bad = Vec::new();
    let mut unreadable = Vec::new();
    let mut dynamic = Vec::new();
    for &shell in ALL_SHELLS {
        let Some((_, state)) = read_rc(shell) else {
            // pwsh/powershell/nu resolve their profile at runtime ($PROFILE,
            // $nu.config-path). Report them as unchecked instead of counting
            // them as healthy.
            dynamic.push(shell);
            continue;
        };
        let contents = match state {
            // Upstream only flags shells whose rc file EXISTS but lacks the
            // plugin; a shell with no rc file at all isn't reported as a
            // missing plugin (avoids false positives for uninstalled shells).
            RcFile::Missing => continue,
            RcFile::Unreadable(e) => {
                unreadable.push((shell, e.to_string()));
                continue;
            }
            RcFile::Contents(bytes) => bytes,
        };
        if !has_insh_rs_plugin(shell, &contents) {
            without.push(shell);
            continue;
        }
        if !plugin_is_last(shell, &contents) {
            bad.push(shell);
        }
    }
    (without, bad, unreadable, dynamic)
}

fn insh_rs_marker() -> &'static str {
    // Whatever string is unique to our install snippet. Matches the
    // MARKER constant in src/shell_init.rs.
    crate::shell_init::wrapper_marker()
}

fn has_insh_rs_plugin(shell: Shell, contents: &[u8]) -> bool {
    crate::shell_init::has_wrapper_block(contents)
        || crate::shell_init::has_source_line(shell, contents)
}

fn plugin_is_last(shell: Shell, contents: &[u8]) -> bool {
    // The last-line checks are textual; a non-UTF-8 profile is still
    // diagnosable because the markers themselves are ASCII.
    let text = String::from_utf8_lossy(contents);
    let source_marker = crate::shell_init::source_marker(shell);
    let wrapper_ok = crate::shell_init::has_wrapper_block(contents)
        && is_last_non_whitespace(&text, insh_rs_marker());
    let source_ok = crate::shell_init::has_source_line(shell, contents)
        && is_last_non_whitespace_line(&text, &source_marker);
    wrapper_ok || source_ok
}

fn is_last_non_whitespace(contents: &str, marker: &str) -> bool {
    let Some(idx) = contents.rfind(marker) else {
        return false;
    };
    // Find the end of the block (the closing MARKER_END).
    let after = &contents[idx + marker.len()..];
    let end_marker = "# <<< inshellisense-rs init <<<";
    let Some(end_rel) = after.find(end_marker) else {
        // Open marker without close — treat as corrupt
        return false;
    };
    let tail = &after[end_rel + end_marker.len()..];
    tail.chars().all(|c| c.is_whitespace())
}

fn is_last_non_whitespace_line(contents: &str, marker: &str) -> bool {
    let Some(idx) = contents.rfind(marker) else {
        return false;
    };
    let tail = &contents[idx + marker.len()..];
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

    /// Captured output must be plain text: `is doctor > report.txt` used to
    /// embed raw ESC bytes in every line, and `NO_COLOR` was ignored.
    #[test]
    fn plain_palette_carries_no_escape_bytes() {
        for (name, value) in [
            ("green_check", PLAIN_PALETTE.green_check),
            ("red_bullet", PLAIN_PALETTE.red_bullet),
            ("red_dash", PLAIN_PALETTE.red_dash),
            ("yellow", PLAIN_PALETTE.yellow),
            ("reset", PLAIN_PALETTE.reset),
            ("bold", PLAIN_PALETTE.bold),
            ("emph", PLAIN_PALETTE.emph),
        ] {
            assert!(
                !value.contains('\x1b'),
                "plain palette field {name} still contains an escape byte"
            );
        }
        assert_eq!(PLAIN_PALETTE.green_check, "✓");
        assert_eq!(PLAIN_PALETTE.red_bullet, "•");
    }

    /// The colored palette is unchanged on a terminal, so users migrating
    /// from upstream still see identical diagnostics.
    #[test]
    fn color_palette_matches_the_upstream_format() {
        assert_eq!(COLOR_PALETTE.green_check, "\x1b[32m✓\x1b[0m");
        assert_eq!(COLOR_PALETTE.red_bullet, "\x1b[31m•\x1b[0m");
        assert_eq!(COLOR_PALETTE.emph, "\x1b[4m\x1b[36m");
    }

    /// NO_COLOR wins whenever it holds a non-empty value — including "0" —
    /// and a non-terminal stream disables color even with it unset.
    #[test]
    fn resolve_color_follows_no_color_and_tty() {
        use std::ffi::OsStr;
        assert!(resolve_color(None, true));
        assert!(!resolve_color(None, false));
        // An empty NO_COLOR is explicitly "color allowed" per no-color.org.
        assert!(resolve_color(Some(OsStr::new("")), true));
        for value in ["1", "0", "false", "yes"] {
            assert!(
                !resolve_color(Some(OsStr::new(value)), true),
                "NO_COLOR={value} must disable color"
            );
        }
    }

    #[test]
    fn last_line_check_positive() {
        let s =
            "foo\nbar\n# >>> inshellisense-rs init >>>\nline\n# <<< inshellisense-rs init <<<\n";
        assert!(is_last_non_whitespace(s, "# >>> inshellisense-rs init >>>"));
    }

    #[test]
    fn last_line_check_negative() {
        let s = "foo\n# >>> inshellisense-rs init >>>\nline\n# <<< inshellisense-rs init <<<\nalias ll=ls\n";
        assert!(!is_last_non_whitespace(
            s,
            "# >>> inshellisense-rs init >>>"
        ));
    }

    /// A commented-out source line is not an installation. Treating it as one
    /// made doctor report a shell as healthy while nothing was sourced.
    #[test]
    fn commented_source_line_is_not_a_plugin() {
        let marker = crate::shell_init::source_marker(Shell::Bash);
        let commented = format!("alias ll=ls\n# {marker}\n");
        assert!(!has_insh_rs_plugin(Shell::Bash, commented.as_bytes()));

        let live = format!("alias ll=ls\n{marker}\n");
        assert!(has_insh_rs_plugin(Shell::Bash, live.as_bytes()));
    }

    /// An unterminated wrapper block is corrupt, not installed.
    #[test]
    fn unterminated_wrapper_block_is_not_a_plugin() {
        let open_only = "# >>> inshellisense-rs init >>>\nexec is start\n";
        assert!(!has_insh_rs_plugin(Shell::Bash, open_only.as_bytes()));
    }

    /// A legacy upstream entry must be reported even when our own current
    /// entry sits alongside it — our snippet also mentions `~/.inshellisense`,
    /// so it has to be stripped before the scan rather than short-circuiting it.
    #[test]
    fn legacy_entry_is_visible_next_to_a_current_one() {
        let ours = crate::shell_init::source_marker(Shell::Bash);
        let legacy = "source ~/.inshellisense/init/bash/init.sh.old";

        let both = format!("{legacy}\n{ours}\n");
        let remainder = crate::shell_init::strip_our_entries(Shell::Bash, both.as_bytes());
        assert!(contains(&remainder, b"~/.inshellisense"));

        // Only our own entry: nothing legacy remains after stripping.
        let mine_only = format!("alias ll=ls\n{ours}\n");
        let remainder = crate::shell_init::strip_our_entries(Shell::Bash, mine_only.as_bytes());
        assert!(!contains(&remainder, b"~/.inshellisense"));
    }

    #[test]
    fn commented_legacy_reference_is_ignored() {
        assert!(is_comment(b"  # source ~/.inshellisense/init/bash/init.sh"));
        assert!(!is_comment(b"source ~/.inshellisense/init/bash/init.sh"));
    }

    #[test]
    fn last_line_check_missing_end() {
        let s = "# >>> inshellisense-rs init >>>\n(no close)\n";
        assert!(!is_last_non_whitespace(
            s,
            "# >>> inshellisense-rs init >>>"
        ));
    }
}
