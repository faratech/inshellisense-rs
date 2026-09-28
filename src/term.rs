//! Headless VT tracking + command manager.
//!
//! We feed the (OSC-6973-stripped) PTY output into a vt100 parser so bash +
//! readline can edit their virtual screen line. Combined with prompt-start /
//! prompt-end markers from the shell integration script we can at any moment
//! answer: "what has the user typed on the current command line?".

use crate::ansi::IsEvent;

pub struct TermTracker {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    cwd: String,
    state: CmdState,
    /// Deferred PromptEnd: ConPTY sends OSC markers in a separate
    /// chunk before the screen-painting bytes, so the cursor is at
    /// (0,0) when PE fires. This flag defers anchor-setting until
    /// after the next batch of bytes updates the vt100 screen.
    pending_prompt_end: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CmdState {
    /// Screen row where the prompt ended — our "start of command" anchor.
    /// `None` before the first prompt is observed. Screen-relative, so it is
    /// re-derived whenever the screen scrolls (see `refresh_command`).
    prompt_end_row: Option<usize>,
    prompt_end_col: Option<usize>,
    /// True between PromptStart and PromptEnd.
    in_prompt: bool,
    /// Last snapshot of the *whole* command line, not just the part before
    /// the cursor.
    pub command: String,
    /// How many characters of `command` sit before the cursor. Equal to
    /// `command.chars().count()` exactly when the cursor is at end-of-line.
    pub cursor_offset: usize,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

impl TermTracker {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
            cwd: String::new(),
            state: CmdState::default(),
            pending_prompt_end: false,
        }
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn state(&self) -> &CmdState {
        &self.state
    }

    pub fn has_prompt_anchor(&self) -> bool {
        self.state.prompt_end_row.is_some()
    }

    /// Fallback for platforms where OSC 6973 markers are stripped
    /// (e.g. Windows ConPTY). Called when the user starts typing
    /// but no PromptEnd has been received — the cursor is sitting
    /// right after the prompt, so its position IS the prompt end.
    pub fn set_fallback_anchor(&mut self) {
        let (r, c) = self.parser.screen().cursor_position();
        self.state.prompt_end_row = Some(r as usize);
        self.state.prompt_end_col = Some(c as usize);
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Rows below the current cursor row, exclusive of the cursor row
    /// itself. Used to decide whether the popup should render below the
    /// prompt (enough room) or flip above it.
    pub fn remaining_lines(&self) -> u16 {
        self.rows
            .saturating_sub(self.state.cursor_row.saturating_add(1))
    }

    /// Is the cursor positioned at the end of the current command
    /// text? Used to suppress ghost-text redraws when the user has
    /// moved their cursor into the middle of their command — in that
    /// case the terminal cells ahead of the cursor are real typed
    /// characters, and writing ghost text over them would corrupt
    /// the line.
    ///
    /// Returns `true` when there's no active command (nothing to be
    /// mid-edit in).
    ///
    /// This compares the cursor against the *full* command line. Comparing it
    /// against the prompt-to-cursor text (which is what `command` used to
    /// hold) made the answer tautologically `true`, so ghost text happily
    /// erased the suffix of a line the user had cursored back into.
    pub fn cursor_at_command_end(&self) -> bool {
        if self.state.prompt_end_row.is_none() {
            return true;
        }
        self.state.cursor_offset == self.state.command.chars().count()
    }

    /// The command text up to the cursor. Completions are computed from what
    /// precedes the cursor, not from the whole line — with the cursor after
    /// `git s` in `git status`, the candidate is `s`, not `status`.
    pub fn command_before_cursor(&self) -> &str {
        if self.state.cursor_offset == 0 {
            return "";
        }
        if self.cursor_at_command_end() {
            return &self.state.command;
        }
        match self
            .state
            .command
            .char_indices()
            .nth(self.state.cursor_offset)
        {
            Some((idx, _)) => &self.state.command[..idx],
            None => &self.state.command,
        }
    }

    /// Called on SIGWINCH — tell the headless vt parser about the new
    /// geometry so cursor tracking stays consistent with the real tty.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
    }

    /// Feed cleaned bytes (no OSC 6973) into the parser and refresh cmd state.
    pub fn feed(&mut self, bytes: &[u8], events: &[IsEvent]) {
        // Debug: dump bytes to /tmp/insh-bytes.log for offline analysis.
        if std::env::var("INSH_DUMP_BYTES").is_ok() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/insh-bytes.log")
            {
                let _ = writeln!(
                    f,
                    "{}",
                    bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
        self.parser.process(bytes);
        // Apply deferred PromptEnd from a PREVIOUS feed() call.
        // ConPTY sends OSC markers before screen-paint bytes, so we
        // must wait for a feed() with actual bytes before reading the
        // cursor position.
        if self.pending_prompt_end && !bytes.is_empty() {
            self.pending_prompt_end = false;
            let (r, c) = self.parser.screen().cursor_position();
            self.state.prompt_end_row = Some(r as usize);
            self.state.prompt_end_col = Some(c as usize);
        }
        for ev in events {
            match ev {
                IsEvent::PromptStart => {
                    self.state.in_prompt = true;
                    self.state.prompt_end_row = None;
                    self.state.prompt_end_col = None;
                    self.state.command.clear();
                    self.pending_prompt_end = false;
                }
                IsEvent::PromptEnd => {
                    // Always defer — on ConPTY the cursor hasn't moved
                    // to the post-prompt position yet. On Unix, bytes
                    // and events arrive together, so the second deferred
                    // check below applies it in the same feed() call.
                    self.state.in_prompt = false;
                    self.pending_prompt_end = true;
                }
                IsEvent::Cwd(p) => self.cwd = p.clone(),
            }
        }
        // Second deferred check: if PE fired in THIS call and bytes
        // were non-empty (Unix — bytes and events in same chunk), the
        // cursor is already at the correct post-prompt position.
        if self.pending_prompt_end && !bytes.is_empty() {
            self.pending_prompt_end = false;
            let (r, c) = self.parser.screen().cursor_position();
            self.state.prompt_end_row = Some(r as usize);
            self.state.prompt_end_col = Some(c as usize);
        }
        self.refresh_command();
    }

    fn clear_anchor(&mut self) {
        self.state.prompt_end_row = None;
        self.state.prompt_end_col = None;
        self.state.command.clear();
        self.state.cursor_offset = 0;
    }

    fn refresh_command(&mut self) {
        let (cursor_row, cursor_col) = self.parser.screen().cursor_position();
        self.state.cursor_row = cursor_row;
        self.state.cursor_col = cursor_col;

        let (Some(pr), Some(pc)) = (self.state.prompt_end_row, self.state.prompt_end_col) else {
            // No anchor — nothing to extract. Keep command empty so
            // renderers don't draw anything.
            self.state.command.clear();
            self.state.cursor_offset = 0;
            return;
        };

        let screen = self.parser.screen();
        // Where does the logical line under the cursor actually begin? A row
        // that wraps into its successor marks the continuation, so walking up
        // through wrapped rows finds the line's first screen row no matter how
        // far the screen has scrolled since the prompt was drawn.
        let first_row = logical_line_start(screen, cursor_row) as usize;

        let pr = if first_row < pr {
            // The screen scrolled: the prompt moved up by `pr - first_row`
            // rows. The stored anchor now points at unrelated text — typing
            // past the bottom row used to reduce the tracked command to just
            // its last wrapped fragment. Re-anchor to where the line really is.
            self.state.prompt_end_row = Some(first_row);
            first_row
        } else if first_row > pr {
            // The cursor is on a *different* logical line than the anchor.
            // This is the stale-anchor case: a nested shell (`sudo su`) that
            // never sourced our integration emits its own prompts, so no fresh
            // PromptStart/End arrives and the old anchor drifts. Extracting
            // from it would feed arbitrary prior output to the suggestion
            // engine. Drop the anchor and wait for a real PromptEnd.
            self.clear_anchor();
            return;
        } else {
            pr
        };

        // A prompt that has scrolled off the top leaves us no way to know how
        // many columns of the first visible row belong to it.
        if pc > self.cols as usize {
            self.clear_anchor();
            return;
        }

        let cols = self.cols as usize;
        let last_row = logical_line_end(screen, pr as u16, self.rows) as usize;

        // Extract the whole line, not merely the part before the cursor.
        let mut cmd = String::new();
        if pr == last_row {
            cmd.push_str(&row_text(screen, pr, pc, cols));
        } else {
            cmd.push_str(&row_text(screen, pr, pc, cols));
            for r in (pr + 1)..last_row {
                cmd.push_str(&row_text(screen, r, 0, cols));
            }
            cmd.push_str(&row_text(screen, last_row, 0, cols));
        }

        // Characters between the anchor and the cursor. Counted in CHARS,
        // not cells: a wide character spans two cells but is one character,
        // and cursor_offset is compared against `cmd.chars().count()` below
        // (#55). Mixing the units padded every CJK line with phantom
        // characters taken from the row's blank padding.
        let chars_between = |row: usize, start: usize, end: usize| -> usize {
            row_char_count(screen, row, start, end.min(cols))
        };
        let cursor_row_us = cursor_row as usize;
        let cursor_offset = if cursor_row_us == pr {
            chars_between(pr, pc, cursor_col as usize)
        } else {
            chars_between(pr, pc, cols)
                + ((pr + 1)..cursor_row_us)
                    .map(|r| chars_between(r, 0, cols))
                    .sum::<usize>()
                + chars_between(cursor_row_us, 0, cursor_col as usize)
        };

        // The row is padded with blanks out to the last column. Trim them, but
        // never past the cursor: a trailing space the user actually typed
        // (`git `) is indistinguishable from padding except by cursor position.
        let typed_len = cmd.trim_end_matches(' ').chars().count();
        let keep = typed_len.max(cursor_offset);
        self.state.command = cmd.chars().take(keep).collect();
        self.state.cursor_offset = cursor_offset.min(self.state.command.chars().count());
    }
}

/// First screen row of the logical line containing `row`, found by walking up
/// through rows that wrap into their successor.
fn logical_line_start(screen: &vt100::Screen, row: u16) -> u16 {
    let mut first = row;
    while first > 0 && screen.row_wrapped(first - 1) {
        first -= 1;
    }
    first
}

/// Last screen row of the logical line beginning at `row`.
fn logical_line_end(screen: &vt100::Screen, row: u16, rows: u16) -> u16 {
    let mut last = row;
    while last + 1 < rows && screen.row_wrapped(last) {
        last += 1;
    }
    last
}

/// Number of visible characters in `row[start_col..end_col]` — wide-char
/// continuation cells count as zero, matching how `row_text` renders them.
fn row_char_count(screen: &vt100::Screen, row: usize, start_col: usize, end_col: usize) -> usize {
    let mut n = 0;
    let max_col = end_col.min(screen.size().1 as usize);
    for col in start_col..max_col {
        if let Some(cell) = screen.cell(row as u16, col as u16)
            && !cell.is_wide_continuation()
        {
            n += 1;
        }
    }
    n
}

fn row_text(screen: &vt100::Screen, row: usize, start_col: usize, end_col: usize) -> String {
    let mut s = String::new();
    let max_col = end_col.min(screen.size().1 as usize);
    for col in start_col..max_col {
        if let Some(cell) = screen.cell(row as u16, col as u16) {
            // A wide (double-width) character occupies its own cell plus a
            // continuation cell whose contents are empty. Turning that
            // continuation cell into a space (as an earlier version did via
            // the empty check below) inserts one phantom space per wide
            // character, corrupting the tracked CJK command line so no
            // suggestion ever matches (#55).
            if cell.is_wide_continuation() {
                continue;
            }
            let contents = cell.contents();
            if contents.is_empty() {
                s.push(' ');
            } else {
                s.push_str(contents);
            }
        } else {
            s.push(' ');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::IsEvent;

    #[test]
    fn preserves_trailing_space_at_cursor() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(b"$ ", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        tracker.feed(b"git ", &[]);
        assert_eq!(tracker.state().command, "git ");
        assert!(tracker.cursor_at_command_end());
    }

    /// A wide (double-width) character occupies two vt100 cells; the
    /// continuation cell used to be rendered as a spurious space, corrupting
    /// the tracked command for CJK input so no suggestion could match (#55).
    #[test]
    fn wide_chars_leave_no_phantom_spaces() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(b"$ ", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        tracker.feed("git add 你好.txt".as_bytes(), &[]);
        assert_eq!(
            tracker.state().command,
            "git add 你好.txt",
            "continuation cells must not become spaces"
        );
    }

    /// Cursoring back into the middle of a line must be detected. The tracker
    /// used to re-extract only the text before the cursor, which made
    /// `cursor_at_command_end()` compare a value against itself and always
    /// return true — so ghost text erased the real `tatus` suffix.
    #[test]
    fn detects_cursor_moved_into_middle_of_line() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(b"$ ", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        tracker.feed(b"git status", &[]);
        assert_eq!(tracker.state().command, "git status");
        assert!(tracker.cursor_at_command_end());

        // Cursor left five columns: now sitting between `git s` and `tatus`.
        tracker.feed(b"\x1b[5D", &[]);
        assert_eq!(
            tracker.state().command,
            "git status",
            "the whole line must still be tracked, not just the prefix"
        );
        assert_eq!(tracker.state().cursor_offset, 5);
        assert!(
            !tracker.cursor_at_command_end(),
            "ghost text would overwrite the `tatus` suffix"
        );
    }

    /// A command typed on the bottom row wraps and scrolls the screen. The
    /// prompt anchor is screen-relative, so it must follow the scroll —
    /// otherwise the tracked command collapses to its last wrapped fragment.
    #[test]
    fn survives_wrap_scroll_on_bottom_row() {
        let mut tracker = TermTracker::new(5, 10);
        // Push the prompt down to the last row.
        tracker.feed(b"\r\n\r\n\r\n\r\n", &[]);
        tracker.feed(b"$ ", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        assert_eq!(tracker.state().cursor_row, 4);

        // 8 chars fill the row, the 9th wraps and scrolls the screen up.
        tracker.feed(b"abcdefghijk", &[]);
        assert_eq!(tracker.state().command, "abcdefghijk");
        assert!(tracker.cursor_at_command_end());
    }

    /// Multi-row wraps of any depth are tracked, not just the two the old
    /// staleness heuristic allowed for.
    #[test]
    fn tracks_command_wrapped_across_several_rows() {
        let mut tracker = TermTracker::new(24, 10);
        tracker.feed(b"$ ", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        // 8 cols on the first row, then 10 per row after: 30 chars spans 4 rows.
        let typed = "abcdefghijklmnopqrstuvwxyz0123";
        tracker.feed(typed.as_bytes(), &[]);
        assert_eq!(tracker.state().command, typed);
        assert!(tracker.cursor_at_command_end());
    }

    /// A nested shell that never sourced our integration emits its own prompts
    /// with no PromptEnd, leaving the anchor pointing at an older line. The
    /// tracker must drop the anchor rather than feed prior output to the
    /// suggestion engine.
    #[test]
    fn drops_stale_anchor_when_cursor_leaves_the_line() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(b"$ ", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        tracker.feed(b"sudo su", &[]);
        assert_eq!(tracker.state().command, "sudo su");

        // Nested shell prints output and its own (unmarked) prompt.
        tracker.feed(b"\r\nroot output\r\nroot# ", &[]);
        assert!(!tracker.has_prompt_anchor());
        assert_eq!(tracker.state().command, "");
    }
}
