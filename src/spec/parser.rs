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

use std::sync::{Arc, OnceLock};

/// Which family of escape/quote rules to tokenize with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellFlavor {
    /// bash/zsh/xonsh/nushell: `'` `"` `` ` `` quote, `\` escapes.
    #[default]
    Posix,
    /// fish: `'` and `"` quote; backslashes can escape single quotes too.
    Fish,
    /// pwsh/PowerShell: `'` `"` quote, `` ` `` escapes (NOT a quote char).
    PowerShell,
    /// cmd.exe: `"` quotes, `^` escapes.
    Cmd,
}

impl ShellFlavor {
    fn esc(self) -> char {
        match self {
            Self::Posix | Self::Fish => '\\',
            Self::PowerShell => '`',
            Self::Cmd => '^',
        }
    }

    fn is_quote(self, ch: char) -> bool {
        match self {
            Self::Posix => ch == '\'' || ch == '"' || ch == '`',
            Self::Fish | Self::PowerShell => ch == '\'' || ch == '"',
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
    /// The shell word before `=` splitting. Required option arguments consume
    /// this word as data, while ordinary options retain the split tokens.
    pub unsplit: Option<Box<UnsplitToken>>,
}

#[derive(Debug, Clone)]
pub struct UnsplitToken {
    word: Arc<str>,
    offset: usize,
    token_length: usize,
    complete: bool,
    parts: usize,
    token: OnceLock<CommandToken>,
}

impl PartialEq for UnsplitToken {
    fn eq(&self, other: &Self) -> bool {
        self.word == other.word
            && self.offset == other.offset
            && self.token_length == other.token_length
            && self.complete == other.complete
            && self.parts == other.parts
    }
}

impl Eq for UnsplitToken {}

impl UnsplitToken {
    pub(crate) fn token(&self) -> &CommandToken {
        self.token.get_or_init(|| CommandToken {
            token: self.word[self.offset..].to_owned(),
            complete: self.complete,
            token_length: self.token_length,
            ..Default::default()
        })
    }

    pub(crate) fn parts(&self) -> usize {
        self.parts
    }
}

/// Parse a command line into tokens for the *last* pipeline segment.
pub fn parse_command(line: &str) -> Vec<CommandToken> {
    parse_command_for(line, ShellFlavor::Posix)
}

/// Tokenize with a specific shell's escape/quote rules (#81).
pub fn parse_command_for(line: &str, flavor: ShellFlavor) -> Vec<CommandToken> {
    let last = last_segment(line, flavor).trim_start();
    let (mut tokens, spans) = lex_with(last, flavor);
    sanitize_with(&mut tokens, flavor);
    retain_unsplit_words(&mut tokens, &spans, last);
    mark_raw_after_dashdash(&mut tokens);
    tokens
}

fn last_segment(line: &str, flavor: ShellFlavor) -> &str {
    &line[segment_start(line, flavor)..]
}

/// Byte index just past the last top-level compound delimiter.
pub(crate) fn segment_start(line: &str, flavor: ShellFlavor) -> usize {
    // Track escape parity while walking forward. Looking backward over every
    // preceding escape run made a long pasted run quadratic (#121).
    let mut chars = line.char_indices().peekable();
    let esc = flavor.esc();
    let mut best = 0;
    let mut quote: Option<char> = None;
    let mut escaped_next = false;
    while let Some((byte, ch)) = chars.next() {
        let escaped = escaped_next;
        escaped_next = ch == esc && !escaped;
        if let Some(q) = quote {
            // POSIX and PowerShell single quotes treat the escape character
            // literally. PowerShell embeds an apostrophe by doubling it.
            if ch == q && (!escaped || (q == '\'' && flavor != ShellFlavor::Fish)) {
                if q == '\''
                    && flavor == ShellFlavor::PowerShell
                    && chars.peek().is_some_and(|(_, next)| *next == '\'')
                {
                    chars.next();
                    escaped_next = false;
                } else {
                    quote = None;
                }
            }
            continue;
        }
        if flavor.is_quote(ch) && !escaped {
            quote = Some(ch);
            continue;
        }
        if !escaped {
            if (ch == '|' || ch == '&') && chars.peek().is_some_and(|(_, next)| *next == ch) {
                let (next_byte, next) = chars.next().unwrap();
                best = next_byte + next.len_utf8();
                escaped_next = false;
            } else if ch == ';' || ch == '|' {
                best = byte + ch.len_utf8();
            }
        }
    }
    best
}

fn lex_with(command: &str, flavor: ShellFlavor) -> (Vec<CommandToken>, Vec<(usize, usize)>) {
    let mut tokens: Vec<CommandToken> = Vec::new();
    let mut spans = Vec::new();
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
    let mut escaped_next = false;
    let mut skip_doubled_quote = false;

    let slice = |start_ch: usize, end_ch: usize| -> String {
        command[byte_pos[start_ch]..byte_pos[end_ch]].to_string()
    };

    for idx in 0..chars.len() {
        let ch = chars[idx];
        let escaped = escaped_next;
        escaped_next = ch == esc && !escaped;
        if skip_doubled_quote {
            skip_doubled_quote = false;
            continue;
        }
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

        let next_char = chars.get(idx + 1).copied();

        if reading_quoted
            && ch == quote_char
            && (!escaped || (quote_char == '\'' && flavor != ShellFlavor::Fish))
        {
            if quote_char == '\'' && flavor == ShellFlavor::PowerShell && next_char == Some('\'') {
                skip_doubled_quote = true;
                continue;
            }
            let next_is_space = next_char.map(|c| c.is_whitespace()).unwrap_or(true);
            if !next_is_space {
                // `"hello"world` — continue reading as quote-continued
                reading_quoted = false;
                reading_quote_continued = true;
            } else {
                reading_quoted = false;
                let complete = idx + 1 < chars.len() && chars[idx + 1].is_whitespace();
                tokens.push(CommandToken {
                    token: decode_quoted(slice(reading_idx + 1, idx), flavor, quote_char),
                    complete,
                    is_option: false,
                    is_quoted: true,
                    // +1 open quote, +1 close quote already in [reading_idx, idx]
                    token_length: idx - reading_idx + 1,
                    ..Default::default()
                });
                spans.push((byte_pos[reading_idx], byte_pos[idx + 1]));
            }
        } else if reading_quote_continued && ch.is_whitespace() && !escaped {
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
            spans.push((byte_pos[reading_idx], byte_pos[idx]));
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
            spans.push((byte_pos[reading_idx], byte_pos[idx]));
            if ch == '=' && idx + 1 == chars.len() {
                tokens.push(CommandToken {
                    token: String::new(),
                    complete: false,
                    is_option: false,
                    token_length: 0,
                    ..Default::default()
                });
                spans.push((command.len(), command.len()));
            }
        } else if reading_cmd && ch.is_whitespace() && !escaped {
            reading_cmd = false;
            tokens.push(CommandToken {
                token: slice(reading_idx, idx),
                complete: true,
                is_option: false,
                token_length: idx - reading_idx,
                ..Default::default()
            });
            spans.push((byte_pos[reading_idx], byte_pos[idx]));
        }
    }

    // flush trailing in-progress token
    let reading = reading_quoted || reading_quote_continued || reading_flag || reading_cmd;
    if reading {
        spans.push((byte_pos[reading_idx], command.len()));
        if reading_quoted {
            tokens.push(CommandToken {
                token: decode_quoted(slice(reading_idx + 1, chars.len()), flavor, quote_char),
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

    (tokens, spans)
}

fn retain_unsplit_words(tokens: &mut [CommandToken], spans: &[(usize, usize)], command: &str) {
    let mut start = 0;
    while start < tokens.len() {
        if !tokens[start].separated {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut word = String::new();
        loop {
            word.push_str(&tokens[end].token);
            if !tokens[end].separated {
                break;
            }
            word.push('=');
            if end + 1 >= tokens.len() || spans[end + 1].0 != spans[end].1 + 1 {
                break;
            }
            end += 1;
        }
        let raw_end = spans[end].1 + usize::from(tokens[end].separated);
        let complete = if tokens[end].separated {
            raw_end < command.len()
        } else {
            tokens[end].complete
        };
        let mut token_length = command[spans[start].0..raw_end].chars().count();
        let word: Arc<str> = word.into();
        let mut offset = 0;
        let group_len = end - start + 1;
        for (part, token) in tokens[start..=end].iter_mut().enumerate() {
            if token.separated {
                token.unsplit = Some(Box::new(UnsplitToken {
                    word: Arc::clone(&word),
                    offset,
                    token_length,
                    complete,
                    parts: group_len - part,
                    token: OnceLock::new(),
                }));
            }
            offset += token.token.len() + usize::from(token.separated);
            token_length =
                token_length.saturating_sub(token.token_length + usize::from(token.separated));
        }
        start = end + 1;
    }
}

fn decode_quoted(token: String, flavor: ShellFlavor, quote: char) -> String {
    if flavor == ShellFlavor::PowerShell && quote == '\'' {
        token.replace("''", "'")
    } else {
        token
    }
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
            let protected = if flavor == ShellFlavor::PowerShell && quote == '\'' {
                t.token.replace("''", &sentinel.to_string())
            } else if quote == '\'' && flavor != ShellFlavor::Fish {
                t.token.clone()
            } else {
                t.token
                    .replace(&format!("{esc}{quote}"), &sentinel.to_string())
            };
            let replaced = protected
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

    #[test]
    fn attached_word_provenance_preserves_spans_without_joining_whitespace() {
        for (line, value, length, parts, complete) in [
            ("cmd --flag=va", "--flag=va", 9, 2, false),
            ("cmd --flag=\"va lue\" ", "--flag=va lue", 15, 2, true),
            ("cmd --flag= value", "--flag=", 7, 1, true),
            ("cmd --outer=--flag=va", "--outer=--flag=va", 17, 3, false),
        ] {
            let tokens = parse_command(line);
            let word = tokens[1].unsplit.as_ref().unwrap();
            assert_eq!(word.token().token, value, "{line}");
            assert_eq!(word.token().token_length, length, "{line}");
            assert_eq!(word.parts(), parts, "{line}");
            assert_eq!(word.token().complete, complete, "{line}");
        }
        let tokens = parse_command("cmd --outer=--flag=va");
        let suffix = tokens[2].unsplit.as_ref().unwrap();
        assert_eq!(suffix.token().token, "--flag=va");
        assert_eq!(suffix.token().token_length, 9);
        assert_eq!(suffix.parts(), 2);
    }

    #[test]
    fn nested_attached_words_share_provenance_storage() {
        let line = format!("cmd {}value", "--flag=".repeat(8_000));
        let tokens = parse_command(&line);
        assert_eq!(tokens.len(), 8_002);
        let first = tokens[1].unsplit.as_ref().unwrap();
        let last = tokens[8_000].unsplit.as_ref().unwrap();
        assert!(Arc::ptr_eq(&first.word, &last.word));
        assert_eq!(last.token().token, "--flag=value");
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
    fn single_quoted_escape_char_is_literal() {
        for (line, flavor) in [
            (r"echo 'a\' | git ch", ShellFlavor::Posix),
            (r"echo 'a\' ; git ch", ShellFlavor::Posix),
            ("echo 'a`' | git ch", ShellFlavor::PowerShell),
        ] {
            let tokens = parse_command_for(line, flavor);
            assert_eq!(
                tokens.iter().map(|t| t.token.as_str()).collect::<Vec<_>>(),
                vec!["git", "ch"],
                "{line}"
            );
        }
        assert_eq!(toks(r"echo 'a\' next"), vec!["echo", "a\\", "next"]);
    }

    #[test]
    fn powershell_doubled_single_quotes_keep_literal_apostrophes() {
        for (line, expected) in [
            ("echo 'it''s | still quoted'", "it's | still quoted"),
            ("echo 'it''s'", "it's"),
            ("echo 'it''s", "it's"),
            ("echo 'it''s'continued", "it'scontinued"),
        ] {
            let tokens = parse_command_for(line, ShellFlavor::PowerShell);
            assert_eq!(tokens.len(), 2, "{line}");
            assert_eq!(tokens[1].token, expected, "{line}");
        }
        let tokens = parse_command_for("echo 'it''s | text' | git ch", ShellFlavor::PowerShell);
        assert_eq!(
            tokens.iter().map(|t| t.token.as_str()).collect::<Vec<_>>(),
            vec!["git", "ch"]
        );
    }

    #[test]
    fn fish_keeps_single_quote_escapes_and_literal_backticks() {
        let tokens = parse_command_for(r"echo 'a\'|b' | git ch", ShellFlavor::Fish);
        assert_eq!(
            tokens.iter().map(|t| t.token.as_str()).collect::<Vec<_>>(),
            vec!["git", "ch"]
        );
        let tokens = parse_command_for(r"echo 'a\' | git ch", ShellFlavor::Fish);
        assert_eq!(tokens[0].token, "echo");
        let tokens = parse_command_for("echo `literal | git ch", ShellFlavor::Fish);
        assert_eq!(tokens[0].token, "git");
    }

    #[test]
    fn long_escape_runs_preserve_separator_parity() {
        for (esc, flavor) in [
            ('\\', ShellFlavor::Posix),
            ('`', ShellFlavor::PowerShell),
            ('^', ShellFlavor::Cmd),
        ] {
            let even = format!("echo {}| git ch", esc.to_string().repeat(16_000));
            assert_eq!(parse_command_for(&even, flavor)[0].token, "git");
            let odd = format!("echo {}| git ch", esc.to_string().repeat(16_001));
            assert_eq!(parse_command_for(&odd, flavor)[0].token, "echo");
        }
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

    /// #100: separators inside quotes are literal text, not delimiters.
    #[test]
    fn quoted_separator_does_not_split_segment() {
        assert_eq!(
            toks("git commit -m \"a;b\" --am"),
            vec!["git", "commit", "-m", "a;b", "--am"]
        );
        assert_eq!(toks("echo \"a|b\" x"), vec!["echo", "a|b", "x"]);
        // A real top-level separator still splits.
        assert_eq!(toks("echo \"a;b\" | grep foo"), vec!["grep", "foo"]);
    }

    /// #101: `"a\\"` closes — the backslash before the quote is itself
    /// escaped, so the following token is a new token, not string content.
    #[test]
    fn escaped_backslash_lets_quote_close() {
        assert_eq!(
            toks(r#"git commit -m "a\\" --am"#),
            vec!["git", "commit", "-m", "a\\\\", "--am"]
        );
        // An odd run still escapes the quote: the string is unterminated.
        let ts = parse_command(r#"git commit -m "a\" --am"#);
        assert!(!ts.last().unwrap().complete);
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
