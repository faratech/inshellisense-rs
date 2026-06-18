//! ANSI constants and OSC-6973 stream filtering.
//!
//! inshellisense uses a custom OSC sequence (ESC ] 6973 ; ... BEL) to mark
//! prompt start/end and report cwd from the shell integration script. The host
//! terminal should not see these sequences — we strip them from the PTY output
//! stream before forwarding to the user's real terminal, and forward their
//! payload to the command manager.

pub const ESC: u8 = 0x1b;
pub const BEL: u8 = 0x07;
pub const ST: &[u8] = &[ESC, b'\\'];

pub const OSC_PS: &str = "6973";
pub const PROMPT_STARTED: &str = "PS";
pub const PROMPT_ENDED: &str = "PE";
pub const CWD: &str = "CWD";

pub const CURSOR_HIDE: &str = "\x1b[?25l";
pub const CURSOR_SHOW: &str = "\x1b[?25h";
pub const RESET: &str = "\x1b[0m";
pub const ERASE_LINE_RIGHT: &str = "\x1b[K";
pub const GREY_FG: &str = "\x1b[38;5;8m";
pub const DIM: &str = "\x1b[2m";
pub const ITALIC: &str = "\x1b[3m";

pub fn save_cursor() -> &'static str {
    "\x1b7"
}
pub fn restore_cursor() -> &'static str {
    "\x1b8"
}

#[derive(Debug, Clone)]
pub enum IsEvent {
    PromptStart,
    PromptEnd,
    Cwd(String),
}

/// Scan a raw byte slice from the PTY, extract any OSC 6973;... sequences,
/// and return the cleaned bytes (to forward to the real terminal) plus a
/// list of events our command manager should observe.
///
/// Also returns the bytes to forward to the vt100 parser — those are the
/// same cleaned bytes (we never let our custom OSCs reach any parser).
/// Win32 input mode enable sequence — Windows Terminal sends this
/// through PTY output and it must be stripped (upstream: ui-root.ts:95).
const WIN32_INPUT_MODE: &[u8] = b"\x1b[?9001h";
const MAX_PENDING: usize = 8192;

pub fn scan(input: &[u8]) -> (Vec<u8>, Vec<IsEvent>) {
    let mut scanner = Scanner::new();
    scanner.scan(input)
}

#[derive(Default)]
pub struct Scanner {
    pending: Vec<u8>,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scan(&mut self, input: &[u8]) -> (Vec<u8>, Vec<IsEvent>) {
        let data = if self.pending.is_empty() {
            input.to_vec()
        } else {
            let mut data = Vec::with_capacity(self.pending.len() + input.len());
            data.extend_from_slice(&self.pending);
            data.extend_from_slice(input);
            self.pending.clear();
            data
        };
        self.scan_combined(&data)
    }

    fn scan_combined(&mut self, input: &[u8]) -> (Vec<u8>, Vec<IsEvent>) {
        let mut out = Vec::with_capacity(input.len());
        let mut events = Vec::new();
        let mut i = 0;
        let needle = b"\x1b]6973;";
        while i < input.len() {
            // Strip Win32 input mode sequence (CSI ?9001h).
            if i + WIN32_INPUT_MODE.len() <= input.len()
                && &input[i..i + WIN32_INPUT_MODE.len()] == WIN32_INPUT_MODE
            {
                i += WIN32_INPUT_MODE.len();
                continue;
            }
            if i + needle.len() <= input.len() && &input[i..i + needle.len()] == needle {
                // Find terminator: BEL (0x07) or ST (ESC \\)
                let start = i + needle.len();
                let mut end = start;
                let mut term_len = 0;
                while end < input.len() {
                    if input[end] == BEL {
                        term_len = 1;
                        break;
                    }
                    if end + 1 < input.len() && input[end] == ESC && input[end + 1] == b'\\' {
                        term_len = 2;
                        break;
                    }
                    end += 1;
                }
                if end >= input.len() {
                    self.keep_pending(&input[i..]);
                    break;
                }
                let payload = &input[start..end];
                if let Ok(s) = std::str::from_utf8(payload) {
                    parse_payload(s, &mut events);
                }
                i = end + term_len;
                continue;
            }
            if is_partial_prefix(&input[i..], needle)
                || is_partial_prefix(&input[i..], WIN32_INPUT_MODE)
            {
                self.keep_pending(&input[i..]);
                break;
            }
            out.push(input[i]);
            i += 1;
        }
        (out, events)
    }

    fn keep_pending(&mut self, bytes: &[u8]) {
        if bytes.len() <= MAX_PENDING {
            self.pending.extend_from_slice(bytes);
        } else {
            self.pending.clear();
        }
    }
}

fn is_partial_prefix(tail: &[u8], full: &[u8]) -> bool {
    tail.len() < full.len() && full.starts_with(tail)
}

fn parse_payload(s: &str, events: &mut Vec<IsEvent>) {
    let (tag, rest) = match s.find(';') {
        Some(idx) => (&s[..idx], &s[idx + 1..]),
        None => (s, ""),
    };
    match tag {
        "PS" => events.push(IsEvent::PromptStart),
        "PE" => events.push(IsEvent::PromptEnd),
        "CWD" => events.push(IsEvent::Cwd(unescape(rest))),
        _ => {}
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => {
                    out.push('\\');
                    i += 2;
                }
                b'x' if i + 3 < bytes.len() => {
                    let hex = &s[i + 2..i + 4];
                    if let Ok(n) = u8::from_str_radix(hex, 16) {
                        out.push(n as char);
                    }
                    i += 4;
                }
                _ => {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_prompt_markers() {
        let input = b"\x1b]6973;PS\x07hello\x1b]6973;PE\x07$ ";
        let (out, events) = scan(input);
        assert_eq!(out, b"hello$ ");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], IsEvent::PromptStart));
        assert!(matches!(events[1], IsEvent::PromptEnd));
    }

    #[test]
    fn scan_parses_cwd() {
        let input = b"\x1b]6973;CWD;/home/user\x07";
        let (_, events) = scan(input);
        assert_eq!(events.len(), 1);
        match &events[0] {
            IsEvent::Cwd(s) => assert_eq!(s, "/home/user"),
            _ => panic!("expected Cwd"),
        }
    }

    #[test]
    fn scan_passes_through_normal_text() {
        let input = b"\x1b[0mhello world\n";
        let (out, events) = scan(input);
        assert_eq!(out, input);
        assert!(events.is_empty());
    }

    #[test]
    fn scanner_buffers_split_osc() {
        let mut scanner = Scanner::new();
        let (out1, events1) = scanner.scan(b"abc\x1b]6973;CWD;/tm");
        assert_eq!(out1, b"abc");
        assert!(events1.is_empty());
        let (out2, events2) = scanner.scan(b"p\x07def");
        assert_eq!(out2, b"def");
        assert_eq!(events2.len(), 1);
        match &events2[0] {
            IsEvent::Cwd(s) => assert_eq!(s, "/tmp"),
            _ => panic!("expected Cwd"),
        }
    }
}
