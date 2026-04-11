//! Render a ghost-text tail suggestion to the real terminal.
//!
//! We emit: save-cursor, erase-to-eol, grey+italic, <tail>, reset,
//! restore-cursor. That way bash's next echo of the user's keystroke lands at
//! the real cursor position and overwrites the ghost — we redraw each pass.

use crate::ansi;
use crate::spec::model::Suggestion;
use std::io::{self, Write};

/// Pick the top suggestion for ghost-text rendering. Assumes the Vec is
/// already sorted by priority DESC / type precedence / name length ASC.
/// Returns the tail of the top suggestion's name relative to `partial`,
/// or None if there's nothing to show.
pub fn pick_top(suggestions: &[Suggestion], partial: &str) -> Option<String> {
    let top = suggestions.first()?;
    if top.name.len() <= partial.len() || !top.name.starts_with(partial) {
        return None;
    }
    Some(top.name[partial.len()..].to_string())
}

pub struct Renderer<W: Write> {
    out: W,
    last: Option<String>,
}

impl<W: Write> Renderer<W> {
    pub fn new(out: W) -> Self {
        Self { out, last: None }
    }

    pub fn draw(&mut self, tail: Option<&str>) -> io::Result<()> {
        let new = tail.map(|s| s.to_string());
        if new == self.last {
            return Ok(());
        }
        self.last = new.clone();
        // Erase any previous ghost, then draw fresh one.
        self.out.write_all(ansi::save_cursor().as_bytes())?;
        self.out.write_all(ansi::ERASE_LINE_RIGHT.as_bytes())?;
        if let Some(tail) = tail {
            self.out
                .write_all(format!("{}{}{}", ansi::GREY_FG, tail, ansi::RESET).as_bytes())?;
        }
        self.out.write_all(ansi::restore_cursor().as_bytes())?;
        self.out.flush()?;
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        if self.last.is_none() {
            return Ok(());
        }
        self.last = None;
        self.out.write_all(ansi::save_cursor().as_bytes())?;
        self.out.write_all(ansi::ERASE_LINE_RIGHT.as_bytes())?;
        self.out.write_all(ansi::restore_cursor().as_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}
