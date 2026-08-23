//! Shell alias loader — loads aliases from bash/zsh and integrates
//! them into the suggestion engine.
//!
//! Port of upstream's `src/runtime/alias.ts`. Only bash and zsh are
//! supported (matching upstream's `aliasSupportedShells`).

use crate::shell::Shell;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// How long the alias subprocess may run. An interactive shell sources the
/// user's rc files, which can block on arbitrary commands; without a bound
/// such a file stalled every suggestion at startup.
const ALIAS_TIMEOUT: Duration = Duration::from_secs(3);

/// Load aliases for the given shell. Returns a map of alias name → expansion.
pub fn load(shell: Shell) -> HashMap<String, String> {
    match shell {
        Shell::Bash => load_bash(),
        Shell::Zsh => load_zsh(),
        _ => HashMap::new(),
    }
}

/// Run `bash -i -c "alias -p"` and parse the output.
/// Format: `alias name='value'`
fn load_bash() -> HashMap<String, String> {
    let text = capture_stdout("bash", &["-i", "-c", "alias -p"], ALIAS_TIMEOUT);
    text.map(|t| parse_posix_aliases(&t)).unwrap_or_default()
}

/// Run `zsh -i -c "alias -L"` and parse the output.
///
/// `-L` lists each alias as a reusable command line, i.e. WITH the `alias `
/// prefix (`alias ll='ls -la'`). Plain `alias` prints bare `name=value`
/// lines whose values are left unquoted whenever quoting is not needed —
/// indistinguishable from the `FOO=bar` status text an rc file echoes.
fn load_zsh() -> HashMap<String, String> {
    let text = capture_stdout("zsh", &["-i", "-c", "alias -L"], ALIAS_TIMEOUT);
    text.map(|t| parse_posix_aliases(&t)).unwrap_or_default()
}

/// Run a program and capture its stdout, giving up after `timeout`.
///
/// The child runs in its own process group so a timed-out shell can be killed
/// together with any grandchild it left holding the pipe.
fn capture_stdout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on a separate thread so the wait can be bounded: `read_to_end`
    // blocks until every write end of the pipe is closed, which a hung
    // grandchild can prevent indefinitely.
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    match rx.recv_timeout(timeout) {
        Ok(bytes) => {
            let _ = child.wait();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        Err(_) => {
            abandon(&mut child);
            None
        }
    }
}

/// Kill a timed-out child (and its process group on Unix), then reap it.
fn abandon(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Parse alias output in either `alias name='value'` or `name=value` format.
///
/// The shell runs interactively, so it sources ~/.bashrc / ~/.zshrc first and
/// the stream mixes real aliases with whatever those files echo. Only lines
/// that actually look like an alias definition become entries:
///
/// - a line carrying the `alias ` prefix (what `bash alias -p` and `zsh
///   alias -L` print) is accepted when its name is identifier-like;
/// - a bare `name=value` line is accepted only when its value is quoted,
///   so rc-file noise such as `PATH=/usr/bin:/bin` or `INSH_RS_PROFILE=work`
///   cannot become a phantom alias ranked above real commands.
fn parse_posix_aliases(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A zsh GLOBAL alias prints as `alias -g name=…`; the flag leaves a
        // space inside what would be the name, which rejects it below.
        // Global aliases expand anywhere in the line, not just as the command
        // word, so they cannot be suggested as one anyway.
        let prefixed = line.starts_with("alias ");
        let line = line.strip_prefix("alias ").unwrap_or(line);
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !is_alias_name(name) {
            continue;
        }
        if !prefixed && !is_quoted(rest) {
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
    map
}

/// Alias names become command words. Restricting them to identifier-like
/// words keeps stray output (`2+2=4`, table borders, banner text) from
/// turning into shortcuts.
fn is_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '?' | ':'))
}

/// Does `value` open and close with the same quote character?
fn is_quoted(value: &str) -> bool {
    value.len() >= 2
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
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

    /// An interactive shell sources the user's rc files before listing
    /// aliases, so arbitrary `FOO=bar` echoes used to become phantom aliases
    /// ranked above real commands. A bare line is only an alias when its
    /// value is quoted.
    #[test]
    fn rc_file_noise_is_not_parsed_as_aliases() {
        let input = concat!(
            "INSH_RS_PROFILE=work\n",
            "PATH=/usr/local/bin:/usr/bin\n",
            "LANG=en_US.UTF-8\n",
            "alias gs='git status'\n",
            "2+2=4\n",
        );
        let map = parse_posix_aliases(input);
        assert_eq!(
            map,
            [("gs".to_string(), "git status".to_string())]
                .into_iter()
                .collect()
        );
    }

    /// `zsh alias -L` prints simple values without quotes — but always with
    /// the `alias ` prefix, which is what makes them recognizable.
    #[test]
    fn zsh_list_form_is_accepted_without_quotes() {
        let map = parse_posix_aliases("alias run-help=man\nalias which-command=whence\n");
        assert_eq!(map.get("run-help").map(String::as_str), Some("man"));
        assert_eq!(map.get("which-command").map(String::as_str), Some("whence"));
    }

    /// Names must be identifier-like: zsh global aliases (`alias -g …`) and
    /// stray banner text stay out of the suggestion list.
    #[test]
    fn non_identifier_names_are_rejected() {
        let map = parse_posix_aliases("alias -g G='| grep'\n'a b'=c\nalias 'we ird'=x\n");
        assert!(map.is_empty(), "got {map:?}");
    }

    /// An rc file that blocks must not stall alias loading forever.
    #[test]
    fn blocking_subprocess_is_abandoned_at_the_timeout() {
        let start = std::time::Instant::now();
        let got = capture_stdout(
            "sh",
            &["-c", "sleep 30; echo late"],
            Duration::from_millis(150),
        );
        assert_eq!(got, None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout was not honored"
        );
    }

    #[test]
    fn subprocess_output_still_arrives() {
        let got = capture_stdout("sh", &["-c", "echo hello"], ALIAS_TIMEOUT);
        assert_eq!(got.as_deref(), Some("hello\n"));
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
