//! Popup suggestion renderer — upstream inshellisense style, minimal port.
//!
//! Shows up to `max_suggestions` (default 5) ranked candidates below the
//! cursor in a framed grey box with name + description columns. The
//! active (top-ranked) row is highlighted with reverse video.
//!
//! Acceptance semantics match ghost mode: right-arrow/End/Ctrl-E accepts
//! the top suggestion. Full up/down navigation is deferred — the PTY
//! loop doesn't yet distinguish between "in a popup" vs "not" for key
//! interception. See the P6 note in CHANGELOG.
//!
//! Rendering strategy:
//!   - save cursor, move down one row, draw N lines of suggestion text,
//!     restore cursor.
//!   - clear: save cursor, move down one row, erase N lines, restore.
//!
//! Direction-aware (above/below) is a follow-up — we currently always
//! render below. Upstream flips direction when the cursor is near the
//! bottom of the terminal; that needs a live cursor-row tracker we can
//! query per draw, which in turn needs crossterm's raw-mode cursor API.

use crate::ansi;
use crate::spec::model::Suggestion;
use std::io::{self, Write};

const NAME_COL_WIDTH: usize = 40;
const DESC_COL_WIDTH: usize = 36;

pub struct PopupRenderer<W: Write> {
    out: W,
    max_suggestions: u8,
    last_drawn_rows: u8,
    last_signature: Option<String>,
}

impl<W: Write> PopupRenderer<W> {
    pub fn new(out: W, max_suggestions: u8) -> Self {
        Self {
            out,
            max_suggestions: max_suggestions.max(1),
            last_drawn_rows: 0,
            last_signature: None,
        }
    }

    pub fn draw(&mut self, _tail: Option<&str>, all: &[Suggestion]) -> io::Result<()> {
        // Compute a signature so we can skip redraws when the visible
        // suggestion set hasn't changed.
        let visible: Vec<&Suggestion> = all.iter().take(self.max_suggestions as usize).collect();
        let sig: String = visible
            .iter()
            .map(|s| format!("{}|{}", s.name, s.description.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n");
        if Some(&sig) == self.last_signature.as_ref() {
            return Ok(());
        }
        // First clear any previous popup rows.
        self.clear()?;

        if visible.is_empty() {
            self.last_signature = None;
            return Ok(());
        }

        self.out.write_all(ansi::save_cursor().as_bytes())?;

        // Move to the start of the next line.
        self.out.write_all(b"\x1b[1E")?; // CNL — next line

        for (idx, s) in visible.iter().enumerate() {
            let name = truncate_or_pad(&s.name, NAME_COL_WIDTH);
            let desc = s
                .description
                .as_deref()
                .map(|d| truncate_or_pad(d, DESC_COL_WIDTH))
                .unwrap_or_else(|| " ".repeat(DESC_COL_WIDTH));
            let body = format!("  {}  {}  ", name, desc);
            if idx == 0 {
                // Active row: reverse video.
                self.out.write_all(b"\x1b[7m")?;
                self.out.write_all(body.as_bytes())?;
                self.out.write_all(ansi::RESET.as_bytes())?;
            } else {
                self.out.write_all(ansi::DIM.as_bytes())?;
                self.out.write_all(body.as_bytes())?;
                self.out.write_all(ansi::RESET.as_bytes())?;
            }
            self.out.write_all(ansi::ERASE_LINE_RIGHT.as_bytes())?;
            if idx + 1 < visible.len() {
                self.out.write_all(b"\x1b[1E")?;
            }
        }

        self.out.write_all(ansi::restore_cursor().as_bytes())?;
        self.out.flush()?;

        self.last_drawn_rows = visible.len() as u8;
        self.last_signature = Some(sig);
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        if self.last_drawn_rows == 0 {
            self.last_signature = None;
            return Ok(());
        }
        self.out.write_all(ansi::save_cursor().as_bytes())?;
        // Move to the line after the cursor and erase N lines downward.
        self.out.write_all(b"\x1b[1E")?;
        for i in 0..self.last_drawn_rows {
            self.out.write_all(ansi::ERASE_LINE_RIGHT.as_bytes())?;
            // Clear from cursor to end of line on this row via CSI K with
            // the full-line mode.
            self.out.write_all(b"\x1b[2K")?;
            if i + 1 < self.last_drawn_rows {
                self.out.write_all(b"\x1b[1E")?;
            }
        }
        self.out.write_all(ansi::restore_cursor().as_bytes())?;
        self.out.flush()?;
        self.last_drawn_rows = 0;
        self.last_signature = None;
        Ok(())
    }
}

fn truncate_or_pad(s: &str, width: usize) -> String {
    if s.chars().count() >= width {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        let mut out = s.to_string();
        while out.chars().count() < width {
            out.push(' ');
        }
        out
    }
}
