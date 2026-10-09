//! Grey ghost-text renderer — the default UI, preserved from phase 0.
//!
//! Emits: save-cursor, erase-to-eol, grey, <tail>, reset, restore-cursor.
//! On the next user keystroke, bash echoes into the same position and
//! overwrites the ghost; the next draw re-paints.

use crate::ansi;
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct GhostRenderer {
    last: Option<String>,
}

/// Cut `tail` to at most `max_cells` display columns.
///
/// A tail longer than the room left on the row wrapped onto the next line,
/// which `clear` (a single erase-to-end-of-line) could not erase — and a wrap
/// on the last row scrolls the terminal, moving the prompt out from under the
/// tracker's anchor. Truncating keeps the ghost on one row.
fn fit_to_width(tail: &str, max_cells: usize) -> &str {
    let mut used = 0;
    for (idx, ch) in tail.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_cells {
            return &tail[..idx];
        }
        used += w;
    }
    tail
}

impl GhostRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `max_cells` is the number of columns left on the cursor's row.
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        tail: Option<&str>,
        max_cells: usize,
    ) -> io::Result<()> {
        // Sanitize before measuring: the tail can be the rest of a filename
        // or generator value, and `fit_to_width` counts control characters
        // as zero width, so an escape sequence would survive truncation and
        // reach the terminal.
        let tail = tail.map(super::printable);
        let new = tail
            .as_deref()
            .map(|s| fit_to_width(s, max_cells))
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if new == self.last {
            return Ok(());
        }
        self.last = new.clone();
        out.write_all(ansi::save_cursor().as_bytes())?;
        out.write_all(ansi::ERASE_LINE_RIGHT.as_bytes())?;
        if let Some(tail) = new.as_deref() {
            out.write_all(format!("{}{}{}", ansi::GREY_FG, tail, ansi::RESET).as_bytes())?;
        }
        out.write_all(ansi::restore_cursor().as_bytes())?;
        out.flush()?;
        Ok(())
    }

    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.last.is_none() {
            return Ok(());
        }
        self.last = None;
        out.write_all(ansi::save_cursor().as_bytes())?;
        out.write_all(ansi::ERASE_LINE_RIGHT.as_bytes())?;
        out.write_all(ansi::restore_cursor().as_bytes())?;
        out.flush()?;
        Ok(())
    }

    pub fn is_visible(&self) -> bool {
        self.last.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tail longer than the room left on the row used to wrap onto the next
    /// line, which `clear`'s single erase-to-end-of-line could not remove.
    #[test]
    fn tail_is_truncated_to_the_room_left_on_the_row() {
        assert_eq!(fit_to_width("eckout", 6), "eckout");
        assert_eq!(fit_to_width("eckout", 3), "eck");
        assert_eq!(fit_to_width("eckout", 0), "");
    }

    /// Wide characters occupy two cells and must never be split in half.
    #[test]
    fn wide_characters_are_not_split() {
        assert_eq!(fit_to_width("日本", 4), "日本");
        assert_eq!(fit_to_width("日本", 3), "日");
        assert_eq!(fit_to_width("日本", 1), "");
    }

    #[test]
    fn draw_emits_nothing_when_no_room_remains() {
        let mut ghost = GhostRenderer::new();
        let mut out = Vec::new();
        ghost.draw(&mut out, Some("eckout"), 0).unwrap();
        assert!(!ghost.is_visible());
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("eckout"));
    }

    /// A tail taken from a filename or generator value must never reach the
    /// terminal as an escape sequence (#85).
    #[test]
    fn draw_never_emits_control_characters_from_the_tail() {
        let mut ghost = GhostRenderer::new();
        let mut out = Vec::new();
        ghost
            .draw(&mut out, Some("x\x1b]0;pwned\x07\x1b[2J\r\n\u{9b}2J"), 80)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("\x1b]0;"),
            "OSC reached the terminal: {text:?}"
        );
        assert!(
            !text.contains("\x1b[2J"),
            "CSI reached the terminal: {text:?}"
        );
        assert!(!text.contains('\x07'));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
        assert!(!text.contains('\u{9b}'), "C1 CSI reached the terminal");
        assert!(text.contains("x?]0;pwned??[2J???2J"), "{text:?}");
    }

    /// Replacement characters are one cell wide, so truncation stays exact.
    #[test]
    fn sanitized_tail_is_measured_after_replacement() {
        let mut ghost = GhostRenderer::new();
        let mut out = Vec::new();
        ghost.draw(&mut out, Some("\x1b[2Jabc"), 3).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(&format!("{}?[2{}", ansi::GREY_FG, ansi::RESET)));
    }

    #[test]
    fn draw_truncates_and_stays_on_one_row() {
        let mut ghost = GhostRenderer::new();
        let mut out = Vec::new();
        ghost.draw(&mut out, Some("eckout"), 3).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("eck"));
        assert!(!text.contains("eckout"));
        assert!(ghost.is_visible());
    }
}
