//! Shell history loader.
//!
//! Reads the active shell's history file (bash, zsh, or fish) and produces a
//! list of unique commands in recency order, newest first. Continuation lines
//! are joined so a multi-line entry is one suggestion rather than several
//! fragments.

use crate::shell::Shell;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 10_000;

/// Load history for the detected shell.
pub fn load() -> Vec<String> {
    load_for(crate::shell::detect())
}

/// Load history for a specific shell.
///
/// The loader used to read `~/.bash_history` unconditionally, so zsh and fish
/// users got no history suggestions at all.
pub fn load_for(shell: Shell) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in candidates(shell) {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let entries = match shell {
            Shell::Zsh => parse_zsh(&contents),
            Shell::Fish => parse_fish(&contents),
            _ => parse_posix(&contents),
        };
        // Newest first.
        for entry in entries.into_iter().rev() {
            if entry.is_empty() {
                continue;
            }
            if seen.insert(entry.clone()) {
                out.push(entry);
            }
            if out.len() >= MAX_ENTRIES {
                return out;
            }
        }
    }
    out
}

/// bash: one command per line, `\` continues onto the next line. Lines
/// starting with `#` are timestamps written by `HISTTIMEFORMAT`.
fn parse_posix(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in contents.lines() {
        if pending.is_none() && (line.trim().is_empty() || line.starts_with('#')) {
            continue;
        }
        let continues = line.ends_with('\\');
        let piece = if continues {
            &line[..line.len() - 1]
        } else {
            line
        };
        match &mut pending {
            Some(acc) => {
                acc.push(' ');
                acc.push_str(piece.trim());
            }
            None => pending = Some(piece.trim().to_string()),
        }
        if !continues {
            if let Some(entry) = pending.take() {
                out.push(entry);
            }
        }
    }
    // An unterminated continuation is still a command.
    if let Some(entry) = pending {
        out.push(entry);
    }
    out
}

/// zsh extended history: `: <start>:<elapsed>;<command>`, with `\`
/// continuations. Plain (non-extended) lines are passed through.
fn parse_zsh(contents: &str) -> Vec<String> {
    let stripped: String = contents
        .lines()
        .map(|line| match line.strip_prefix(':') {
            Some(rest) => rest.split_once(';').map(|(_, cmd)| cmd).unwrap_or(line),
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    parse_posix(&stripped)
}

/// fish stores YAML-ish records; the command lives on a `- cmd:` line and may
/// contain escaped newlines.
fn parse_fish(contents: &str) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- cmd:"))
        .map(|cmd| cmd.trim().replace("\\n", " "))
        .collect()
}

fn candidates(shell: Shell) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(hf) = std::env::var("HISTFILE") {
        if !hf.is_empty() {
            v.push(PathBuf::from(hf));
        }
    }
    let Some(home) = crate::paths::home() else {
        return v;
    };
    match shell {
        Shell::Zsh => v.push(home.join(".zsh_history")),
        Shell::Fish => {
            let base = crate::paths::config_dir()
                .map(|c| c.join("fish"))
                .unwrap_or_else(|| home.join(".config/fish"));
            v.push(base.join("fish_history"));
            v.push(home.join(".local/share/fish/fish_history"));
        }
        _ => v.push(home.join(".bash_history")),
    }
    v
}

/// Most recent modification time across the shell's history files. `Engine`
/// uses this to notice when a new command has been written and reload, rather
/// than serving the snapshot taken when the process started.
pub fn revision(shell: Shell) -> Option<std::time::SystemTime> {
    candidates(shell)
        .into_iter()
        .filter_map(|p| fs::metadata(p).ok()?.modified().ok())
        .max()
}

/// Rank history entries by a "starts-with then contains" heuristic and
/// return the first full entry whose prefix matches the current line, if any.
pub fn best_match<'a>(history: &'a [String], line: &str) -> Option<&'a str> {
    if line.is_empty() {
        return None;
    }
    // Exact prefix match wins: newest first.
    for entry in history {
        if entry.starts_with(line) && entry.len() > line.len() {
            return Some(entry.as_str());
        }
    }
    None
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    /// A backslash continuation is one command, not two suggestions. The old
    /// loader pushed every physical line separately, contradicting its own
    /// module docs.
    #[test]
    fn posix_joins_continuation_lines() {
        let text = "ls -la\ndocker run \\\n  --rm alpine\necho done\n";
        assert_eq!(
            parse_posix(text),
            vec!["ls -la", "docker run --rm alpine", "echo done"]
        );
    }

    #[test]
    fn posix_skips_histtimeformat_stamps() {
        let text = "#1700000000\nls -la\n#1700000001\ngit status\n";
        assert_eq!(parse_posix(text), vec!["ls -la", "git status"]);
    }

    /// zsh's extended format prefixes each entry with `: <start>:<elapsed>;`.
    #[test]
    fn zsh_strips_extended_metadata() {
        let text = ": 1700000000:0;git status\n: 1700000001:0;cargo build \\\n  --release\n";
        assert_eq!(parse_zsh(text), vec!["git status", "cargo build --release"]);
    }

    #[test]
    fn zsh_passes_through_plain_lines() {
        assert_eq!(parse_zsh("ls -la\n"), vec!["ls -la"]);
    }

    /// fish stores YAML-ish records; only `- cmd:` lines are commands.
    #[test]
    fn fish_reads_cmd_records() {
        let text = "- cmd: git status\n  when: 1700000000\n- cmd: cargo test\n  when: 1700000001\n";
        assert_eq!(parse_fish(text), vec!["git status", "cargo test"]);
    }
}
