//! Shell alias loader — loads aliases from bash/zsh and integrates
//! them into the suggestion engine.
//!
//! Port of upstream's `src/runtime/alias.ts`. Only bash and zsh are
//! supported (matching upstream's `aliasSupportedShells`).

use crate::shell::Shell;
use std::collections::HashMap;

/// Load aliases for the given shell. Returns a map of alias name → expansion.
pub fn load(shell: Shell) -> HashMap<String, String> {
    match shell {
        Shell::Bash => load_bash(),
        Shell::Zsh => load_zsh(),
        _ => HashMap::new(),
    }
}

/// Run `bash -i -c "alias"` and parse the output.
/// Format: `alias name='value'`
fn load_bash() -> HashMap<String, String> {
    let output = std::process::Command::new("bash")
        .args(["-i", "-c", "alias"])
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse_posix_aliases(&text)
}

/// Run `zsh -i -c "alias"` and parse the output.
/// Format: `name=value` (no `alias` prefix in zsh)
fn load_zsh() -> HashMap<String, String> {
    let output = std::process::Command::new("zsh")
        .args(["-i", "-c", "alias"])
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse_posix_aliases(&text)
}

/// Parse alias output in either `alias name='value'` or `name=value` format.
fn parse_posix_aliases(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        // Strip optional "alias " prefix (bash includes it, zsh doesn't).
        let line = line.strip_prefix("alias ").unwrap_or(line);
        if let Some((name, rest)) = line.split_once('=') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            // Strip surrounding quotes from the value.
            let value = rest
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .or_else(|| rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                .unwrap_or(rest);
            map.insert(name.to_string(), value.to_string());
        }
    }
    map
}

/// Expand aliases in a command line. If the first word is an alias,
/// replace it with the alias value.
pub fn expand(line: &str, aliases: &HashMap<String, String>) -> String {
    let first = line.split_whitespace().next().unwrap_or("");
    if let Some(expansion) = aliases.get(first) {
        let rest = line[first.len()..].to_string();
        format!("{}{}", expansion, rest)
    } else {
        line.to_string()
    }
}

/// Expand an alias only in the active command segment. Shells apply aliases
/// after separators such as `&&` and `|`; this mirrors the parser's simple
/// "last segment" model.
pub fn expand_active_segment(line: &str, aliases: &HashMap<String, String>) -> String {
    let segment_start = last_segment_start(line);
    let leading_ws = line[segment_start..]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| line[segment_start..].len());
    let word_start = segment_start + leading_ws;
    let Some((word_len, word)) = first_word(&line[word_start..]) else {
        return line.to_string();
    };
    let Some(expansion) = aliases.get(word) else {
        return line.to_string();
    };
    let mut out = String::with_capacity(line.len() - word.len() + expansion.len());
    out.push_str(&line[..word_start]);
    out.push_str(expansion);
    out.push_str(&line[word_start + word_len..]);
    out
}

fn first_word(s: &str) -> Option<(usize, &str)> {
    let end = s
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| s.len());
    if end == 0 {
        None
    } else {
        Some((end, &s[..end]))
    }
}

fn last_segment_start(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut best_idx = 0;
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len()
            && ((bytes[i] == b'|' && bytes[i + 1] == b'|')
                || (bytes[i] == b'&' && bytes[i + 1] == b'&'))
        {
            best_idx = i + 2;
            i += 2;
            continue;
        }
        if bytes[i] == b';' || bytes[i] == b'|' {
            best_idx = i + 1;
        }
        i += 1;
    }
    best_idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bash_format() {
        let input = "alias ll='ls -la'\nalias gs='git status'\n";
        let map = parse_posix_aliases(input);
        assert_eq!(map.get("ll").unwrap(), "ls -la");
        assert_eq!(map.get("gs").unwrap(), "git status");
    }

    #[test]
    fn parse_zsh_format() {
        let input = "ll='ls -la'\ngs='git status'\n";
        let map = parse_posix_aliases(input);
        assert_eq!(map.get("ll").unwrap(), "ls -la");
        assert_eq!(map.get("gs").unwrap(), "git status");
    }

    #[test]
    fn expand_alias() {
        let mut aliases = HashMap::new();
        aliases.insert("gs".into(), "git status".into());
        assert_eq!(expand("gs -s", &aliases), "git status -s");
        assert_eq!(expand("ls -la", &aliases), "ls -la");
    }

    #[test]
    fn parse_escaped_quotes() {
        // Bash format with escaped single quotes:
        // alias la='echo '\''lo'\'' '\''la'\'''
        let input = "alias la='echo '\\''lo'\\'' '\\''la'\\'''";
        let map = parse_posix_aliases(input);
        assert!(map.contains_key("la"));
    }

    #[test]
    fn expand_no_match() {
        let aliases = HashMap::new();
        assert_eq!(expand("git status", &aliases), "git status");
    }

    #[test]
    fn expand_with_flags() {
        let mut aliases = HashMap::new();
        aliases.insert("glo".into(), "git log --oneline".into());
        assert_eq!(expand("glo --all", &aliases), "git log --oneline --all");
    }

    #[test]
    fn expand_active_segment_after_separator() {
        let mut aliases = HashMap::new();
        aliases.insert("gs".into(), "git status".into());
        assert_eq!(
            expand_active_segment("echo ok && gs -s", &aliases),
            "echo ok && git status -s"
        );
    }
}
