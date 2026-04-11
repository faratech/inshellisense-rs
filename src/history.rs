//! Bash history loader.
//!
//! Reads ~/.bash_history (and, if present, $HISTFILE) and produces a list of
//! unique commands in recency order, newest first. Multi-line history entries
//! (written with cmdhist/lithist) are joined with spaces.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

pub fn load() -> Vec<String> {
    let paths = candidates();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        // Iterate in reverse so newest lines come first.
        for line in contents.lines().rev() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if seen.insert(line.to_string()) {
                out.push(line.to_string());
            }
            if out.len() >= 10_000 {
                return out;
            }
        }
    }
    out
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(hf) = std::env::var("HISTFILE") {
        v.push(PathBuf::from(hf));
    }
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".bash_history"));
    }
    v
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
