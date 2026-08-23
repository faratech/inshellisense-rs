//! `cli` parity category — compare the flag and subcommand name SETS
//! between upstream and ours, not the raw help text.
//!
//! Commander.js (upstream) and clap (ours) produce wildly different
//! help text formatting — brackets, column widths, "Usage:" lines,
//! description wrapping. Diffing raw text gives 100% noise. Instead,
//! we extract the set of `-x` / `--long-flag` tokens and the set of
//! subcommand names from each binary's `--help` output, then compare
//! set membership. This surfaces the signal (missing/extra flags,
//! missing/extra subcommands) without drowning in formatting.

use super::{Category, CategoryReport, ScanConfig};
use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

const DEFAULT_INVOCATIONS: &[&str] = &[
    "--help",
    "complete --help",
    "init --help",
    "specs list --help",
    "doctor --help",
    "reinit --help",
];

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Cli);

    // Both binaries run against the same scratch HOME, so the operator's
    // spec sources and config cannot configure our side alone.
    let home = super::isolated_home(cfg);

    let invocations = load_invocations(&cfg.corpus_dir.join("cli.txt"))
        .unwrap_or_else(|_| DEFAULT_INVOCATIONS.iter().map(|s| s.to_string()).collect());

    for invocation in &invocations {
        let args: Vec<&str> = invocation.split_whitespace().collect();
        let ours_out = run_and_capture(&cfg.ours, &home, &args);
        let upstream_out = run_and_capture(&cfg.upstream, &home, &args);

        let ours_flags = extract_flags(&ours_out);
        let upstream_flags = extract_flags(&upstream_out);
        let ours_subs = extract_subcommands(&ours_out);
        let upstream_subs = extract_subcommands(&upstream_out);

        let missing_flags: Vec<&str> = upstream_flags
            .difference(&ours_flags)
            .map(|s| s.as_str())
            .collect();
        let missing_subs: Vec<&str> = upstream_subs
            .difference(&ours_subs)
            .map(|s| s.as_str())
            .collect();

        if missing_flags.is_empty() && missing_subs.is_empty() {
            report.push_pass(invocation);
            continue;
        }

        let mut details = Vec::new();
        if !missing_flags.is_empty() {
            details.push(format!(
                "missing flags: {}",
                missing_flags
                    .iter()
                    .take(10)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !missing_subs.is_empty() {
            details.push(format!(
                "missing subcommands: {}",
                missing_subs
                    .iter()
                    .take(10)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let reason = if !missing_flags.is_empty() {
            format!("missing {} upstream flags", missing_flags.len())
        } else {
            format!("missing {} upstream subcommands", missing_subs.len())
        };
        report.push_fail(invocation, reason, details, 60);
    }

    report.summary = format!(
        "{}/{} help invocations match (flag + subcommand set)",
        report.pass_count(),
        report.cases.len()
    );
    report
}

fn load_invocations(path: &std::path::Path) -> std::io::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

/// A spawn failure must never be comparable to another spawn failure — the
/// sentinel embeds the binary path so `ours` and `upstream` differ and the
/// case fails loudly instead of matching.
fn run_and_capture(bin: &std::path::Path, home: &std::path::Path, args: &[&str]) -> String {
    let mut cmd = Command::new(bin);
    super::deterministic(&mut cmd, home);
    let output = cmd.args(args).output();
    match output {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            s
        }
        Err(e) => format!("<error: {} failed to spawn: {}>", bin.display(), e),
    }
}

/// Extract flag names (`-x`, `--long-flag`) from help text.
fn extract_flags(s: &str) -> BTreeSet<String> {
    let no_ansi = strip_ansi(s);
    let mut out = BTreeSet::new();
    for line in no_ansi.lines() {
        for tok in tokenize_flags(line) {
            out.insert(tok);
        }
    }
    out
}

fn tokenize_flags(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '-' {
            let start = i;
            let saw_dash_dash = i + 1 < chars.len() && chars[i + 1] == '-';
            if saw_dash_dash {
                i += 2;
            } else {
                i += 1;
            }
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '-' || chars[i] == '_')
            {
                i += 1;
            }
            let slice: String = chars[start..i].iter().collect();
            if saw_dash_dash {
                if slice.len() >= 3 {
                    out.push(slice);
                }
            } else if slice.len() == 2 && slice.chars().nth(1).unwrap().is_ascii_alphabetic() {
                out.push(slice);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Extract subcommand names from the "Commands:" section of help text.
fn extract_subcommands(s: &str) -> BTreeSet<String> {
    let no_ansi = strip_ansi(s);
    let mut out = BTreeSet::new();
    let mut in_commands = false;
    for line in no_ansi.lines() {
        let trimmed = line.trim_start();
        if trimmed.to_lowercase().starts_with("commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() {
            in_commands = false;
            continue;
        }
        if !line.starts_with("  ") {
            in_commands = false;
            continue;
        }
        if let Some(name) = line.split_whitespace().next()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            out.insert(name.to_string());
        }
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let c = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
