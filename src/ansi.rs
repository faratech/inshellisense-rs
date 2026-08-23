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
                    self.keep_pending(&input[i..], &mut out);
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
                self.keep_pending(&input[i..], &mut out);
                break;
            }
            out.push(input[i]);
            i += 1;
        }
        (out, events)
    }

    /// Buffer a partial sequence for the next chunk. If the buffer would
    /// exceed MAX_PENDING, this cannot be a real marker any more (markers
    /// are a few dozen bytes), so forward the oldest overflow to the terminal
    /// instead of discarding it — silently dropping PTY output corrupted the
    /// display whenever a program emitted `\x1b]6973;` without a terminator,
    /// or a burst arrived while a marker straddled many chunks (#56).
    fn keep_pending(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_PENDING {
            let flush = self.pending.len() - MAX_PENDING;
            out.extend_from_slice(&self.pending[..flush]);
            self.pending.drain(..flush);
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

fn hex_byte(pair: &[u8]) -> Option<u8> {
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    Some(digit(pair[0])? << 4 | digit(pair[1])?)
}

/// Reverse the shell integrations' `__is_escape_value`. The wire format is a
/// *byte* stream — non-ASCII characters travel as their raw UTF-8 bytes, and
/// only `\`, `;`, ESC, BEL and LF are escaped as `\\` / `\xNN`. So unescaping
/// must rebuild the byte sequence and decode it as UTF-8 at the end; decoding
/// each byte to a `char` individually would turn `café` into `cafÃ©`.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'x' if i + 3 < bytes.len() => {
                    // Index bytes, not `&s[..]`: a malformed `\x` followed by
                    // a multi-byte character would panic on a str slice.
                    if let Some(n) = hex_byte(&bytes[i + 2..i + 4]) {
                        out.push(n);
                        i += 4;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    /// Non-ASCII cwds travel as raw UTF-8 bytes (see `__is_escape_value` in
    /// the shell integrations), so they must round-trip unchanged.
    #[test]
    fn scan_parses_non_ascii_cwd() {
        let input = "\x1b]6973;CWD;/tmp/café\x07".as_bytes();
        let (_, events) = scan(input);
        match &events[0] {
            IsEvent::Cwd(s) => assert_eq!(s, "/tmp/café"),
            _ => panic!("expected Cwd"),
        }
    }

    /// The same path escaped byte-wise as `\xNN` must decode identically.
    #[test]
    fn unescape_rebuilds_utf8_from_hex_escapes() {
        assert_eq!(unescape("/tmp/caf\\xc3\\xa9"), "/tmp/café");
        assert_eq!(unescape("a\\x3bb"), "a;b");
        assert_eq!(unescape("a\\\\b"), "a\\b");
        // 日本語 passes through as raw bytes.
        assert_eq!(unescape("/tmp/日本語"), "/tmp/日本語");
    }

    /// A stray `\x` before a multi-byte character must not panic on a str slice.
    #[test]
    fn unescape_tolerates_malformed_hex_escape() {
        assert_eq!(unescape("\\xé"), "\\xé");
        assert_eq!(unescape("\\xZZtail"), "\\xZZtail");
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

    /// An unterminated OSC whose buffered tail outgrew MAX_PENDING used to be
    /// cleared wholesale — that output never reached the terminal (#56). The
    /// overflow must instead be forwarded, keeping only the newest bytes
    /// buffered.
    #[test]
    fn pending_overflow_is_forwarded_not_dropped() {
        let mut scanner = Scanner::new();
        // Open an OSC and stream far more than MAX_PENDING without a terminator.
        let (mut out, events) = scanner.scan(b"\x1b]6973;CWD;/tmp");
        assert!(events.is_empty());
        let chunk = vec![b'x'; 4096];
        for _ in 0..4 {
            let (o, e) = scanner.scan(&chunk);
            out.extend(o);
            assert!(e.is_empty());
        }
        // 3 * 8192 + 14 bytes of payload: everything but the retained tail
        // must have been forwarded, not silently discarded.
        assert!(
            out.len() >= 2 * MAX_PENDING,
            "overflow must reach the terminal (got {} bytes)",
            out.len()
        );
        // The scanner stays usable afterwards: a complete marker resolves.
        let (out2, events2) = scanner.scan(b"tail\x1b]6973;PS\x07done");
        assert!(out2.starts_with(b"tail"));
        assert_eq!(events2.len(), 1);
    }
}
