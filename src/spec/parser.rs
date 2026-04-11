//! Tokenizer — Rust port of inshellisense's lex() from
//! /tmp/inshellisense/src/runtime/parser.ts.
//!
//! The state machine is preserved exactly so parity tests hold. Four active
//! states: reading-quoted, reading-quote-continued, reading-flag, reading-cmd.
//! Long options with `=` (`--foo=bar`) split at `=`. Combined shorts stay
//! joined. Unclosed quotes at EOL emit a token with `complete = false`.
//!
//! We take only the last segment of a compound command (split on `||`/`&&`/
//! `;`/`|`) for suggestion purposes, matching parseCommand().

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
}

/// Parse a command line into tokens for the *last* pipeline segment.
pub fn parse_command(line: &str) -> Vec<CommandToken> {
    let last = last_segment(line).trim_start();
    let mut tokens = lex(last);
    sanitize(&mut tokens);
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
        if i + 1 < bytes.len() && (bytes[i] == b'|' && bytes[i + 1] == b'|'
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

fn lex(command: &str) -> Vec<CommandToken> {
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

    let esc = '\\'; // bash whitespace escape char

    let slice = |start_ch: usize, end_ch: usize| -> String {
        command[byte_pos[start_ch]..byte_pos[end_ch]].to_string()
    };

    for idx in 0..chars.len() {
        let ch = chars[idx];
        let reading = reading_quoted || reading_quote_continued || reading_flag || reading_cmd;

        if !reading && (ch == '\'' || ch == '"' || ch == '`') {
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
                ..Default::default()
            });
        } else if (reading_flag && ch.is_whitespace()) || ch == '=' {
            // Matches inshellisense parser.ts:100 exactly — an unguarded `=`
            // splits the current token and marks it as an option. In the
            // readingFlag case that's the intended `--foo=bar` semantics.
            reading_flag = false;
            tokens.push(CommandToken {
                token: slice(reading_idx, idx),
                complete: true,
                is_option: true,
                ..Default::default()
            });
        } else if reading_cmd && ch.is_whitespace() && prev_char != Some(esc) {
            reading_cmd = false;
            tokens.push(CommandToken {
                token: slice(reading_idx, idx),
                complete: true,
                is_option: false,
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
                ..Default::default()
            });
        } else if reading_quote_continued {
            tokens.push(CommandToken {
                token: slice(reading_idx, chars.len()),
                complete: false,
                is_option: false,
                is_quoted: true,
                is_quote_continued: true,
                ..Default::default()
            });
        } else {
            tokens.push(CommandToken {
                token: slice(reading_idx, chars.len()),
                complete: false,
                is_option: reading_flag,
                ..Default::default()
            });
        }
    }

    tokens
}

fn sanitize(tokens: &mut [CommandToken]) {
    // Unescape `\ ` → ` ` in non-quoted tokens, and unwrap quote-continued
    // tokens (strip the embedded quote chars).
    for t in tokens.iter_mut() {
        if !t.is_quoted && t.token.contains("\\ ") {
            t.token = t.token.replace("\\ ", " ");
        }
        if t.is_quote_continued && !t.token.is_empty() {
            let quote = t.token.chars().next().unwrap();
            let qs = quote.to_string();
            // \" → (sentinel) → "" → unchanged → (sentinel) → "
            let sentinel = '\u{1b}';
            let replaced = t
                .token
                .replace(&format!("\\{quote}"), &sentinel.to_string())
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
}
