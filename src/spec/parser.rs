//! Tokenizer — Rust port of inshellisense's lex() from
//! upstream inshellisense's `src/runtime/parser.ts`.
//!
//! The state machine is preserved exactly so parity tests hold. Four active
//! states: reading-quoted, reading-quote-continued, reading-flag, reading-cmd.
//! Long options with `=` (`--foo=bar`) split at `=` — but only while a flag
//! is being read; unlike upstream parser.ts (which splits ANY word at `=`,
//! killing completion for `VAR=value cmd` lines) we gate the split on the
//! flag state (#53). Combined shorts stay joined. Unclosed quotes at EOL emit
//! a token with `complete = false`.
//!
//! Escape and quote characters are shell-dependent (#81): POSIX shells use
//! `\`, PowerShell uses `` ` `` (and does not treat it as a quote), cmd uses
//! `^`. The default remains the POSIX behavior for parity tests.
//!
//! We take only the last segment of a compound command (split on `||`/`&&`/
//! `;`/`|`) for suggestion purposes, matching parseCommand().

/// Which family of escape/quote rules to tokenize with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellFlavor {
    /// bash/zsh/fish/xonsh/nushell: `'` `"` `` ` `` quote, `\` escapes.
    #[default]
    Posix,
    /// pwsh/PowerShell: `'` `"` quote, `` ` `` escapes (NOT a quote char).
    PowerShell,
    /// cmd.exe: `"` quotes, `^` escapes.
    Cmd,
}

impl ShellFlavor {
    fn esc(self) -> char {
        match self {
            Self::Posix => '\\',
            Self::PowerShell => '`',
            Self::Cmd => '^',
        }
    }

    fn is_quote(self, ch: char) -> bool {
        match self {
            Self::Posix => ch == '\'' || ch == '"' || ch == '`',
            Self::PowerShell => ch == '\'' || ch == '"',
            // cmd.exe has no single-quote string syntax; `'…'` stays literal.
            Self::Cmd => ch == '"',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandToken {
    pub token: String,
    pub complete: bool,
    pub is_option: bool,
    pub is_quoted: bool,
    pub is_quote_continued: bool,
    pub is_persistent: bool,
    /// Set on every token that follows a bare `--` in the stream. Treated as
    /// positional-only by the resolver (a small improvement beyond the
    /// upstream TS parser).
    pub is_raw: bool,
    /// Number of characters this token occupies on the command line,
    /// INCLUDING surrounding quote characters (`"checkout` → 9, `"a b"` → 5).
    /// Differs from `token.len()` for quoted/escaped tokens. Mirrors
    /// upstream's `tokenLength` used for replacement math.
    pub token_length: usize,
    /// Set on an option token whose value was attached with `=`
    /// (`--opt=value`). Distinguishes it from `--opt value`, which the
    /// resolver must not treat as an attached value for a
    /// `requiresSeparator` option.
    pub separated: bool,
}

/// Parse a command line into tokens for the *last* pipeline segment.
pub fn parse_command(line: &str) -> Vec<CommandToken> {
    parse_command_for(line, ShellFlavor::Posix)
}

/// Tokenize with a specific shell's escape/quote rules (#81).
pub fn parse_command_for(line: &str, flavor: ShellFlavor) -> Vec<CommandToken> {
    let last = last_segment(line).trim_start();
    let mut tokens = lex_with(last, flavor);
    sanitize_with(&mut tokens, flavor);
    mark_raw_after_dashdash(&mut tokens);
    tokens
}

fn last_segment(line: &str) -> &str {
    // Split on top-level compound delimiters. We want the last segment,
    // matching parser.ts:23-24. We deliberately avoid quoting/escaping
    // subtleties — inshellisense itself does a naive regex split here.
    let mut best_idx = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // `||` `&&`
        if i + 1 < bytes.len()
            && (bytes[i] == b'|' && bytes[i + 1] == b'|'
                || bytes[i] == b'&' && bytes[i + 1] == b'&')
        {
            best_idx = i + 2;
            i += 2;
            continue;
        }
        // single `;` or `|`
        if c == b';' || c == b'|' {
            best_idx = i + 1;
        }
        i += 1;
    }
    &line[best_idx..]
}

fn lex_with(command: &str, flavor: ShellFlavor) -> Vec<CommandToken> {
    let mut tokens: Vec<CommandToken> = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let byte_pos: Vec<usize> = {
        // map char index -> byte index for slicing the original str
        let mut v = Vec::with_capacity(chars.len() + 1);
        let mut b = 0;
        for c in command.chars() {
            v.push(b);
            b += c.len_utf8();
        }
        v.push(b);
        v
    };

    let mut reading_quoted = false;
    let mut reading_quote_continued = false;
    let mut reading_flag = false;
    let mut reading_cmd = false;
    let mut reading_idx: usize = 0;
    let mut quote_char: char = ' ';

    let esc = flavor.esc();

    let slice = |start_ch: usize, end_ch: usize| -> String {
        command[byte_pos[start_ch]..byte_pos[end_ch]].to_string()
    };

    for idx in 0..chars.len() {
        let ch = chars[idx];
        let reading = reading_quoted || reading_quote_continued || reading_flag || reading_cmd;

        if !reading && flavor.is_quote(ch) {
            reading_quoted = true;
            reading_idx = idx;
            quote_char = ch;
            continue;
        } else if !reading && ch == '-' {
            reading_flag = true;
            reading_idx = idx;
            continue;
        } else if !reading && !ch.is_whitespace() {
            reading_cmd = true;
            reading_idx = idx;
            continue;
        }

        let prev_char = if idx > 0 { Some(chars[idx - 1]) } else { None };
        let next_char = chars.get(idx + 1).copied();

        if reading_quoted && ch == quote_char && prev_char != Some(esc) {
            let next_is_space = next_char.map(|c| c.is_whitespace()).unwrap_or(true);
            if !next_is_space {
                // `"hello"world` — continue reading as quote-continued
                reading_quoted = false;
                reading_quote_continued = true;
            } else {
                reading_quoted = false;
                let complete = idx + 1 < chars.len() && chars[idx + 1].is_whitespace();
                tokens.push(CommandToken {
                    token: slice(reading_idx + 1, idx),
                    complete,
                    is_option: false,
                    is_quoted: true,
                    // +1 open quote, +1 close quote already in [reading_idx, idx]
                    token_length: idx - reading_idx + 1,
                    ..Default::default()
                });
            }
        } else if reading_quote_continued && ch.is_whitespace() && prev_char != Some(esc) {
            reading_quote_continued = false;
            tokens.push(CommandToken {
                token: slice(reading_idx, idx),
                complete: true,
                is_option: false,
                is_quoted: true,
                is_quote_continued: true,
                token_length: idx - reading_idx,
                ..Default::default()
            });
        } else if reading_flag && (ch.is_whitespace() || ch == '=') {
            // Upstream parser.ts:100 splits on an unguarded `=`, so any word
            // containing one (e.g. `FOO=bar` in `env FOO=bar git ch`) is
            // chopped into a phantom option plus an overlapping duplicate,
            // and completion returns nothing (#53). We deliberately diverge:
            // only a flag in progress splits at `=`, which is the intended
            // `--foo=bar` semantics.
            reading_flag = false;
            tokens.push(CommandToken {
                token: slice(reading_idx, idx),
                complete: true,
                is_option: true,
                token_length: idx - reading_idx,
                // Only an `=` attaches the value to the option token.
                separated: ch == '=',
                ..Default::default()
            });
            if ch == '=' && idx + 1 == chars.len() {
                tokens.push(CommandToken {
                    token: String::new(),
                    complete: false,
                    is_option: false,
                    token_length: 0,
                    ..Default::default()
                });
            }
        } else if reading_cmd && ch.is_whitespace() && prev_char != Some(esc) {
            reading_cmd = false;
            tokens.push(CommandToken {
                token: slice(reading_idx, idx),
                complete: true,
                is_option: false,
                token_length: idx - reading_idx,
                ..Default::default()
            });
        }
    }

    // flush trailing in-progress token
    let reading = reading_quoted || reading_quote_continued || reading_flag || reading_cmd;
    if reading {
        if reading_quoted {
            tokens.push(CommandToken {
                token: slice(reading_idx + 1, chars.len()),
                complete: false,
                is_option: false,
                is_quoted: true,
                // unclosed: only the opening quote counts toward the span
                token_length: chars.len() - reading_idx,
                ..Default::default()
            });
        } else if reading_quote_continued {
            tokens.push(CommandToken {
                token: slice(reading_idx, chars.len()),
                complete: false,
                is_option: false,
                is_quoted: true,
                is_quote_continued: true,
                token_length: chars.len() - reading_idx,
                ..Default::default()
            });
        } else {
            tokens.push(CommandToken {
                token: slice(reading_idx, chars.len()),
                complete: false,
                is_option: reading_flag,
                token_length: chars.len() - reading_idx,
                ..Default::default()
            });
        }
    }

    tokens
}

fn sanitize_with(tokens: &mut [CommandToken], flavor: ShellFlavor) {
    // Unescape `<esc> ` → ` ` in non-quoted tokens, and unwrap quote-
    // continued tokens (strip the embedded quote chars). The escape char is
    // shell-dependent (#81): `\` on POSIX, `` ` `` in PowerShell, `^` in cmd.
    let esc = flavor.esc();
    let esc_space = format!("{esc} ");
    for t in tokens.iter_mut() {
        if !t.is_quoted && t.token.contains(&esc_space) {
            t.token = t.token.replace(&esc_space, " ");
        }
        if t.is_quote_continued && !t.token.is_empty() {
            let quote = t.token.chars().next().unwrap();
            let qs = quote.to_string();
            // \" → (sentinel) → "" → unchanged → (sentinel) → "
            let sentinel = '\u{1b}';
            let replaced = t
                .token
                .replace(&format!("{esc}{quote}"), &sentinel.to_string())
                .replace(&qs, "")
                .replace(&sentinel.to_string(), &qs);
            t.token = replaced;
        }
    }
}

fn mark_raw_after_dashdash(tokens: &mut [CommandToken]) {
    let mut after = false;
    for t in tokens.iter_mut() {
        if after {
            t.is_raw = true;
            t.is_option = false;
        } else if t.token == "--" && t.complete {
            after = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(line: &str) -> Vec<String> {
        parse_command(line).into_iter().map(|t| t.token).collect()
    }

    #[test]
    fn simple_split() {
        assert_eq!(toks("git status"), vec!["git", "status"]);
    }

    #[test]
    fn long_option_with_equals_splits() {
        // inshellisense behavior: --foo=bar → ["--foo", "bar"]
        let got = toks("cargo build --target=wasm32");
        assert_eq!(got, vec!["cargo", "build", "--target", "wasm32"]);
    }

    /// A `=` inside a plain word must NOT split it into a phantom option plus
    /// an overlapping duplicate (#53): `env FOO=bar git ch` used to tokenize
    /// as [env, FOO(option), FOO=bar, git, ch] and completion died.
    #[test]
    fn equals_in_plain_word_stays_whole() {
        let tokens = parse_command("env FOO=bar git ch ");
        let names: Vec<&str> = tokens.iter().map(|t| t.token.as_str()).collect();
        assert_eq!(names, vec!["env", "FOO=bar", "git", "ch"]);
        assert!(tokens[1].complete && !tokens[1].is_option);
    }

    /// PowerShell escapes whitespace with a backtick, not a backslash (#81).
    #[test]
    fn powershell_backtick_escape() {
        let tokens = parse_command_for("Get-ChildItem my` file", ShellFlavor::PowerShell);
        let last = tokens.last().unwrap();
        assert_eq!(last.token, "my file");
        assert!(!last.complete);
        // …and the backtick is not a quote character in PowerShell.
        let tokens = parse_command_for("echo `x", ShellFlavor::PowerShell);
        assert_eq!(tokens.last().unwrap().token, "`x");
    }

    /// cmd.exe escapes whitespace with a caret (#81).
    #[test]
    fn cmd_caret_escape() {
        let tokens = parse_command_for("dir my^ file", ShellFlavor::Cmd);
        let last = tokens.last().unwrap();
        assert_eq!(last.token, "my file");
        assert!(!last.complete);
        // Single quotes are literal characters in cmd.exe, not quotes.
        let tokens = parse_command_for("dir 'a b'", ShellFlavor::Cmd);
        let names: Vec<&str> = tokens.iter().map(|t| t.token.as_str()).collect();
        assert_eq!(names, vec!["dir", "'a", "b'"]);
    }

    #[test]
    fn combined_shorts_stay_joined() {
        assert_eq!(toks("tar -xzf a.tar.gz"), vec!["tar", "-xzf", "a.tar.gz"]);
    }

    #[test]
    fn unclosed_quote_is_incomplete() {
        let got = parse_command("git commit -m \"hello");
        assert_eq!(got.last().unwrap().token, "hello");
        assert!(!got.last().unwrap().complete);
    }

    #[test]
    fn trailing_space_emits_no_tail_token() {
        let ts = parse_command("git ");
        assert_eq!(ts.len(), 1);
        assert!(ts[0].complete);
    }

    #[test]
    fn partial_word_incomplete() {
        let ts = parse_command("git ch");
        assert_eq!(ts.len(), 2);
        assert!(!ts[1].complete);
    }

    #[test]
    fn pipe_takes_last_segment() {
        assert_eq!(toks("ls -la | grep foo"), vec!["grep", "foo"]);
    }

    #[test]
    fn double_dash_marks_raw() {
        let ts = parse_command("git log -- file1 file2");
        assert!(ts.iter().find(|t| t.token == "file1").unwrap().is_raw);
        assert!(ts.iter().find(|t| t.token == "file2").unwrap().is_raw);
    }

    #[test]
    fn quoted_string() {
        let ts = parse_command("git commit -m \"hello world\"");
        let msg = ts.last().unwrap();
        assert_eq!(msg.token, "hello world");
        assert!(msg.is_quoted);
    }

    // ── Upstream parser.test.ts parity (63 cases) ──────────────

    #[test]
    fn flag_with_value() {
        assert_eq!(toks("cmd --flag value"), vec!["cmd", "--flag", "value"]);
    }

    #[test]
    fn flag_equals_value() {
        assert_eq!(toks("cmd --flag=value"), vec!["cmd", "--flag", "value"]);
    }

    #[test]
    fn flag_equals_single_quoted() {
        assert_eq!(toks("cmd --flag='value' "), vec!["cmd", "--flag", "value"]);
    }

    #[test]
    fn flag_equals_double_quoted() {
        assert_eq!(
            toks("cmd --flag=\"value\" "),
            vec!["cmd", "--flag", "value"]
        );
    }

    #[test]
    fn single_quoted_arg() {
        assert_eq!(toks("cmd 'value' "), vec!["cmd", "value"]);
    }

    #[test]
    fn bare_value_arg() {
        assert_eq!(toks("cmd value "), vec!["cmd", "value"]);
    }

    #[test]
    fn short_flag_alone() {
        let ts = parse_command("cmd -f");
        assert_eq!(ts.len(), 2);
        assert!(ts[1].is_option);
    }

    #[test]
    fn short_flag_equals_value() {
        assert_eq!(toks("cmd -f=value "), vec!["cmd", "-f", "value"]);
    }

    #[test]
    fn short_flag_space_value() {
        assert_eq!(toks("cmd -f value "), vec!["cmd", "-f", "value"]);
    }

    #[test]
    fn short_flag_space_single_quoted() {
        assert_eq!(toks("cmd -f 'value' "), vec!["cmd", "-f", "value"]);
    }

    #[test]
    fn short_flag_equals_double_quoted() {
        assert_eq!(toks("cmd -f=\"value\" "), vec!["cmd", "-f", "value"]);
    }

    #[test]
    fn short_flag_equals_incomplete_quote() {
        let ts = parse_command("cmd -f='val");
        assert_eq!(ts.last().unwrap().token, "val");
        assert!(!ts.last().unwrap().complete);
    }

    #[test]
    fn short_flag_trailing_space() {
        let ts = parse_command("cmd -f ");
        assert_eq!(ts.len(), 2);
        assert!(ts[1].complete);
    }

    #[test]
    fn single_command() {
        let ts = parse_command("cmd");
        assert_eq!(ts.len(), 1);
        assert!(!ts[0].complete);
    }

    #[test]
    fn single_command_trailing_space() {
        let ts = parse_command("cmd ");
        assert_eq!(ts.len(), 1);
        assert!(ts[0].complete);
    }

    #[test]
    fn mixed_quotes_double_then_single() {
        let ts = parse_command("cmd \"value' ");
        // Unclosed double quote containing a single quote
        assert!(!ts.last().unwrap().complete);
    }

    #[test]
    fn double_quoted_value() {
        assert_eq!(toks("cmd \"value\" "), vec!["cmd", "value"]);
    }

    #[test]
    fn pipe_two_commands() {
        assert_eq!(toks("cmd1 | cmd2 "), vec!["cmd2"]);
    }

    #[test]
    fn command_with_dash() {
        let ts = parse_command("cmd1 -");
        assert_eq!(ts.last().unwrap().token, "-");
        assert!(ts.last().unwrap().is_option);
    }

    #[test]
    fn quote_continued_no_space() {
        // "item1"item2 — quote-continued token
        let ts = parse_command("cmd1 \"item1\"item2");
        assert!(ts.len() >= 2);
    }

    #[test]
    fn quote_continued_with_following_arg() {
        let ts = parse_command("cmd1 \"item1\"item2 item3");
        assert!(ts.len() >= 3);
    }

    #[test]
    fn emoji_input() {
        let ts = parse_command("\u{1f601}");
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].token, "\u{1f601}");
    }

    #[test]
    fn empty_input() {
        let ts = parse_command("");
        assert!(ts.is_empty());
    }

    #[test]
    fn whitespace_only() {
        let ts = parse_command("   ");
        assert!(ts.is_empty());
    }

    #[test]
    fn command_trailing_spaces() {
        let ts = parse_command("cmd   ");
        assert_eq!(ts.len(), 1);
        assert!(ts[0].complete);
    }

    #[test]
    fn multi_operator_chain() {
        // cmd1 | cmd2 && cmd3 ; cmd4 → last segment is cmd4
        assert_eq!(toks("cmd1 | cmd2 && cmd3 ; cmd4"), vec!["cmd4"]);
    }

    #[test]
    fn or_then_pipe() {
        // cmd1 || cmd2 | cmd3 → last segment is cmd3
        assert_eq!(toks("cmd1 || cmd2 | cmd3"), vec!["cmd3"]);
    }

    #[test]
    fn tab_in_command() {
        let ts = parse_command("cmd\targ");
        assert!(ts.len() >= 2);
    }

    #[test]
    fn empty_single_quotes() {
        assert_eq!(toks("cmd '' "), vec!["cmd", ""]);
    }

    #[test]
    fn empty_double_quotes() {
        assert_eq!(toks("cmd \"\" "), vec!["cmd", ""]);
    }

    #[test]
    fn incomplete_single_quote() {
        let ts = parse_command("cmd 'incomplete");
        assert!(!ts.last().unwrap().complete);
    }

    #[test]
    fn incomplete_double_quote() {
        let ts = parse_command("cmd \"incomplete");
        assert!(!ts.last().unwrap().complete);
    }

    #[test]
    fn flag_equals_no_value() {
        let ts = parse_command("cmd --flag=");
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[1].token, "--flag");
        assert!(ts[1].is_option);
        assert_eq!(ts[2].token, "");
        assert!(!ts[2].complete);
    }

    #[test]
    fn flag_equals_empty_single_quotes() {
        assert_eq!(toks("cmd --flag=''"), vec!["cmd", "--flag", ""]);
    }

    #[test]
    fn flag_equals_empty_double_quotes() {
        assert_eq!(toks("cmd --flag=\"\""), vec!["cmd", "--flag", ""]);
    }

    #[test]
    fn short_flag_equals_empty() {
        let ts = parse_command("cmd -f=");
        assert_eq!(ts[0].token, "cmd");
        assert_eq!(ts[1].token, "-f");
        assert_eq!(ts[2].token, "");
        assert!(!ts[2].complete);
    }

    #[test]
    fn two_quoted_args() {
        assert_eq!(
            toks("cmd 'hello' \"world\" "),
            vec!["cmd", "hello", "world"]
        );
    }

    #[test]
    fn its_quote() {
        // "it's" — double-quoted string containing single quote
        assert_eq!(toks("cmd \"it's\" "), vec!["cmd", "it's"]);
    }

    #[test]
    fn quote_continued_multi() {
        // "a"b "c"d — two quote-continued tokens
        let ts = parse_command("cmd \"a\"b \"c\"d");
        assert!(ts.len() >= 3);
    }
}
