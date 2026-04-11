//! Grey ghost-text renderer — the default UI, preserved from phase 0.
//!
//! Emits: save-cursor, erase-to-eol, grey, <tail>, reset, restore-cursor.
//! On the next user keystroke, bash echoes into the same position and
//! overwrites the ghost; the next draw re-paints.

use crate::ansi;
use std::io::{self, Write};

pub struct GhostRenderer<W: Write> {
    out: W,
    last: Option<String>,
}

impl<W: Write> GhostRenderer<W> {
    pub fn new(out: W) -> Self {
        Self { out, last: None }
    }

    pub fn draw(&mut self, tail: Option<&str>) -> io::Result<()> {
        let new = tail.map(|s| s.to_string());
        if new == self.last {
            return Ok(());
        }
        self.last = new.clone();
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
