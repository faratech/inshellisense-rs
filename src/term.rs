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
    /// Whether prompt paint bytes arrived between PromptStart and
    /// PromptEnd. If they did, the cursor at PromptEnd is already the
    /// post-prompt position and must be snapshotted then; deferring to
    /// the next chunk anchors after typeahead echo (#102).
    saw_prompt_paint: bool,
    /// `INSH_DUMP_BYTES` debug transcript, opened once at startup.
    byte_dump: Option<std::fs::File>,
}

#[derive(Debug, Clone, Default)]
pub struct CmdState {
    /// First visible row occupied by the entire prompt, including preceding
    /// lines of a multiline prompt. Popup rows must stay outside this region.
    prompt_start_row: Option<u16>,
    /// Screen row where the prompt ended — our "start of command" anchor.
    /// `None` before the first prompt is observed. Screen-relative, so it is
    /// re-derived whenever the screen scrolls (see `refresh_command`).
    prompt_end_row: Option<usize>,
    prompt_end_col: Option<usize>,
    /// Logical line containing the command anchor. A wrapped prompt can start
    /// this line before `prompt_end_row`; that difference is not scrolling.
    command_line_start_row: Option<u16>,
    command_last_row: u16,
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
            saw_prompt_paint: false,
            byte_dump: open_byte_dump(std::env::var_os("INSH_DUMP_BYTES").as_deref()),
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
        self.state.prompt_start_row = Some(logical_line_start(self.parser.screen(), r));
        self.state.prompt_end_row = Some(r as usize);
        self.state.prompt_end_col = Some(c as usize);
        self.state.command_line_start_row = Some(logical_line_start(self.parser.screen(), r));
        self.state.command_last_row = logical_line_end(self.parser.screen(), r, self.rows);
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

    /// Inclusive screen rows occupied by the current prompt and whole command.
    /// Cursor movement within a wrapped command must not expose its suffix (or
    /// earlier prompt lines) as space that a popup can draw over and clear.
    pub fn popup_region(&self) -> (u16, u16) {
        let cursor = self.state.cursor_row;
        let first = self.state.prompt_start_row.unwrap_or(cursor).min(cursor);
        let last = self.state.command_last_row.max(cursor).min(self.rows - 1);
        (first.min(self.rows - 1), last)
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
        let (r, c) = self.parser.screen().cursor_position();
        self.state.cursor_row = r;
        self.state.cursor_col = c;
        self.state.prompt_start_row = self.state.prompt_start_row.map(|r| r.min(rows - 1));
        self.state.command_last_row = self.state.command_last_row.min(rows - 1);
        self.state.prompt_end_row = self
            .state
            .prompt_end_row
            .map(|r| r.min((rows - 1) as usize));
        self.state.command_line_start_row = self
            .state
            .prompt_end_row
            .map(|r| logical_line_start(self.parser.screen(), r as u16));
    }

    /// Feed cleaned bytes (no OSC 6973) into the parser and refresh cmd state.
    pub fn feed(&mut self, bytes: &[u8], events: &[IsEvent]) {
        self.dump_bytes(bytes);
        self.process(bytes, events);
        self.refresh_command();
    }

    /// Apply markers at their original positions in the cleaned stream.
    /// Refresh the command only once after all spans, avoiding repeated full
    /// line extraction when one read contains several prompt markers.
    pub fn feed_ordered(&mut self, bytes: &[u8], events: &[(usize, IsEvent)]) {
        self.dump_bytes(bytes);
        let mut previous = 0;
        for (offset, event) in events {
            let offset = (*offset).clamp(previous, bytes.len());
            self.process(&bytes[previous..offset], &[]);
            self.process(&[], std::slice::from_ref(event));
            previous = offset;
        }
        self.process(&bytes[previous..], &[]);
        self.refresh_command();
    }

    fn dump_bytes(&mut self, bytes: &[u8]) {
        // Debug: dump bytes (INSH_DUMP_BYTES) for offline analysis.
        if let Some(f) = self.byte_dump.as_mut() {
            use std::io::Write;
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

    fn process(&mut self, bytes: &[u8], events: &[IsEvent]) {
        // The legacy feed API attaches PromptStart to the prompt paint itself.
        // Capture its starting position before processing those bytes too.
        let starts_prompt = events.iter().any(|ev| matches!(ev, IsEvent::PromptStart));
        if starts_prompt {
            self.state.prompt_start_row = Some(self.parser.screen().cursor_position().0);
        }
        if self.state.in_prompt && !bytes.is_empty() {
            self.saw_prompt_paint = true;
        }
        if self.state.in_prompt || starts_prompt || self.pending_prompt_end {
            self.process_prompt_paint(bytes);
        } else {
            self.parser.process(bytes);
        }
        // Apply deferred PromptEnd from a PREVIOUS feed() call.
        // ConPTY sends OSC markers before screen-paint bytes, so we
        // must wait for a feed() with actual bytes before reading the
        // cursor position.
        if self.pending_prompt_end && !bytes.is_empty() {
            self.pending_prompt_end = false;
            let (r, c) = self.parser.screen().cursor_position();
            self.state.prompt_end_row = Some(r as usize);
            self.state.prompt_end_col = Some(c as usize);
            self.state.command_line_start_row = Some(logical_line_start(self.parser.screen(), r));
        }
        for ev in events {
            match ev {
                IsEvent::PromptStart => {
                    self.state.in_prompt = true;
                    self.state.prompt_end_row = None;
                    self.state.prompt_end_col = None;
                    self.state.command_line_start_row = None;
                    self.state.command.clear();
                    self.pending_prompt_end = false;
                    // Bytes in this same chunk are prompt paint.
                    self.saw_prompt_paint = !bytes.is_empty();
                }
                IsEvent::PromptEnd => {
                    self.state.in_prompt = false;
                    if self.saw_prompt_paint {
                        // Paint already processed: snapshot the cursor
                        // as of PromptEnd. Deferring would anchor after
                        // the typeahead echo in the next chunk (#102).
                        let (r, c) = self.parser.screen().cursor_position();
                        self.state.prompt_end_row = Some(r as usize);
                        self.state.prompt_end_col = Some(c as usize);
                        self.state.command_line_start_row =
                            Some(logical_line_start(self.parser.screen(), r));
                        self.pending_prompt_end = false;
                    } else {
                        // ConPTY: markers arrive before the paint bytes,
                        // so defer until the next non-empty chunk.
                        self.pending_prompt_end = true;
                    }
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
            self.state.command_line_start_row = Some(logical_line_start(self.parser.screen(), r));
        }
    }

    /// Follow prompt rows when painting at the bottom scrolls them upward.
    /// Split only at line feeds: wrapping stays in the same logical line and
    /// its first visible row exposes the scroll distance. A line feed at the
    /// bottom accounts for the additional scroll into a new logical line.
    fn process_prompt_paint(&mut self, bytes: &[u8]) {
        for span in bytes.split_inclusive(|b| *b == b'\n') {
            let newline = span.last() == Some(&b'\n');
            let paint = if newline {
                &span[..span.len() - 1]
            } else {
                span
            };
            let before = logical_line_start(
                self.parser.screen(),
                self.parser.screen().cursor_position().0,
            );
            self.parser.process(paint);
            let after = logical_line_start(
                self.parser.screen(),
                self.parser.screen().cursor_position().0,
            );
            let mut scrolled = before.saturating_sub(after);
            if newline {
                let before = self.parser.screen().cursor_position().0;
                self.parser.process(b"\n");
                if before == self.rows - 1 && self.parser.screen().cursor_position().0 == before {
                    scrolled = scrolled.saturating_add(1);
                }
            }
            if let Some(row) = &mut self.state.prompt_start_row {
                *row = row.saturating_sub(scrolled);
            }
        }
    }

    fn clear_anchor(&mut self) {
        self.state.prompt_end_row = None;
        self.state.prompt_end_col = None;
        self.state.command_line_start_row = None;
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

        let line_start = self.state.command_line_start_row.unwrap_or(pr as u16) as usize;
        let pr = if first_row < line_start {
            // The screen scrolled: the prompt moved up by `line_start - first_row`
            // rows. The stored anchor now points at unrelated text — typing
            // past the bottom row used to reduce the tracked command to just
            // its last wrapped fragment. Re-anchor to where the line really is.
            let scrolled = line_start - first_row;
            let anchor = pr.saturating_sub(scrolled);
            self.state.prompt_end_row = Some(anchor);
            self.state.command_line_start_row = Some(first_row as u16);
            if let Some(row) = &mut self.state.prompt_start_row {
                *row = row.saturating_sub(scrolled as u16);
            }
            anchor
        } else if first_row > line_start {
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
        self.state.command_last_row = last_row as u16;

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

/// Open the `INSH_DUMP_BYTES` debug transcript, or `None` when it is unset
/// or cannot be opened safely.
///
/// The transcript holds every byte the wrapped shell prints. It used to be
/// appended to the fixed, shared `/tmp/insh-bytes.log` with default
/// permissions (world-readable under a 022 umask), through whatever file or
/// symlink another local user had put there first. It now goes to
/// `~/.inshellisense/insh-bytes.log`, or to the path the variable names when
/// that is absolute (`INSH_DUMP_BYTES=1` keeps working). On Unix the file is
/// opened without following a final symlink, created 0600, and used only if
/// it is a regular file owned by this user; an existing one is tightened to
/// 0600.
fn open_byte_dump(setting: Option<&std::ffi::OsStr>) -> Option<std::fs::File> {
    let setting = setting?;
    let path = if std::path::Path::new(setting).is_absolute() {
        std::path::PathBuf::from(setting)
    } else {
        let root = crate::paths::resource_root()?;
        std::fs::create_dir_all(&root).ok()?;
        root.join("insh-bytes.log")
    };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file.metadata().ok()?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } {
            return None;
        }
        if meta.mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .ok()?;
        }
    }
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::IsEvent;

    /// The `INSH_DUMP_BYTES` transcript is private: created 0600, never
    /// written through a planted symlink, and an existing file is tightened
    /// before anything is appended (#88).
    #[cfg(unix)]
    #[test]
    fn byte_dump_is_private_and_never_follows_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::unique_temp_dir("dump");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // A fresh file is created 0600 and receives the dump.
        let log = dir.join("bytes.log");
        let mut tracker = TermTracker::new(24, 80);
        tracker.byte_dump = open_byte_dump(Some(log.as_os_str()));
        tracker.feed(b"hi", &[]);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "68 69\n");
        assert_eq!(mode(&log), 0o600);

        // A symlink planted at the path is refused; its target is untouched.
        let target = dir.join("victim");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.join("link.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_byte_dump(Some(link.as_os_str())).is_none());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        // A world-readable file left by an older build is tightened.
        let old = dir.join("old.log");
        std::fs::write(&old, "").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_byte_dump(Some(old.as_os_str())).is_some());
        assert_eq!(mode(&old), 0o600);

        // Unset: no transcript at all.
        assert!(open_byte_dump(None).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #102: PromptEnd in its own chunk after the paint must anchor at
    /// the prompt end, so typeahead echo in the next chunk is tracked.
    #[test]
    fn typeahead_after_split_prompt_end_is_tracked() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(b"$ ", &[IsEvent::PromptStart]);
        tracker.feed(b"", &[IsEvent::PromptEnd]);
        tracker.feed(b"git", &[]);
        assert_eq!(tracker.state().command, "git");
        // ConPTY control: markers before any paint still defer to paint.
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(b"", &[IsEvent::PromptStart, IsEvent::PromptEnd]);
        tracker.feed(b"$ ", &[]);
        tracker.feed(b"git", &[]);
        assert_eq!(tracker.state().command, "git");
    }

    #[test]
    fn ordered_prompt_and_typeahead_are_independent_of_read_boundaries() {
        let wire =
            "prior\r\n\x1b]6973;CWD;/tmp/café\x07\x1b]6973;PS\x07$ \x1b]6973;PE\x07git add café "
                .as_bytes();
        let check = |chunks: Vec<&[u8]>| {
            let mut scanner = crate::ansi::Scanner::new();
            let mut tracker = TermTracker::new(24, 80);
            for chunk in chunks {
                let (bytes, events) = scanner.scan_ordered(chunk);
                tracker.feed_ordered(&bytes, &events);
            }
            assert_eq!(tracker.state().command, "git add café ");
            assert_eq!(tracker.cwd(), "/tmp/café");
            assert!(tracker.cursor_at_command_end());
        };
        // Covers a fully coalesced read, every possible two-read split
        // (inside markers, their ST/BEL terminators and UTF-8), and bytewise
        // delivery. PromptEnd must anchor before the typed echo in each case.
        for split in 0..=wire.len() {
            check(vec![&wire[..split], &wire[split..]]);
        }
        check(wire.chunks(1).collect());
    }

    #[test]
    fn ordered_markers_preserve_conpty_deferred_paint() {
        for coalesced_paint in [false, true] {
            let mut scanner = crate::ansi::Scanner::new();
            let mut tracker = TermTracker::new(24, 80);
            let wire = if coalesced_paint {
                b"\x1b]6973;PS\x07\x1b]6973;PE\x07$ ".as_slice()
            } else {
                b"\x1b]6973;PS\x07\x1b]6973;PE\x07".as_slice()
            };
            let (bytes, events) = scanner.scan_ordered(wire);
            tracker.feed_ordered(&bytes, &events);
            if !coalesced_paint {
                tracker.feed_ordered(b"$ ", &[]);
            }
            tracker.feed_ordered(b"git", &[]);
            assert_eq!(tracker.state().command, "git");
        }
    }

    #[test]
    fn ordered_multiple_prompts_in_one_read_track_only_the_latest_command() {
        let mut scanner = crate::ansi::Scanner::new();
        let mut tracker = TermTracker::new(24, 80);
        let (bytes, events) = scanner.scan_ordered(b"\x1b]6973;PS\x07$ \x1b]6973;PE\x07old\r\noutput\r\n\x1b]6973;PS\x07$ \x1b]6973;PE\x07git");
        tracker.feed_ordered(&bytes, &events);
        assert_eq!(tracker.state().command, "git");
    }

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

    #[test]
    fn popup_region_includes_multiline_prompt_and_command_suffix() {
        let mut tracker = TermTracker::new(12, 10);
        tracker.feed_ordered(b"\r\n\r\n", &[]);
        tracker.feed_ordered(
            b"first\r\n$ abcdefghijklmnopq",
            &[(0, IsEvent::PromptStart), (9, IsEvent::PromptEnd)],
        );
        assert_eq!(tracker.state().command, "abcdefghijklmnopq");
        assert_eq!(tracker.popup_region(), (2, 4));
        tracker.feed(b"\x1b[A\r\x1b[4C", &[]);
        assert_eq!(tracker.state().cursor_row, 3);
        assert_eq!(tracker.popup_region(), (2, 4));
        assert_eq!(tracker.state().command, "abcdefghijklmnopq");
    }

    #[test]
    fn wrapped_prompt_is_not_mistaken_for_scrolling() {
        let mut tracker = TermTracker::new(10, 10);
        tracker.feed_ordered(
            b"\r\nabcdefghijkl$ git",
            &[(2, IsEvent::PromptStart), (16, IsEvent::PromptEnd)],
        );
        assert_eq!(tracker.state().command, "git");
        assert_eq!(tracker.popup_region(), (1, 2));
    }

    #[test]
    fn popup_region_follows_prompt_and_command_scroll() {
        let mut tracker = TermTracker::new(5, 10);
        tracker.feed_ordered(b"\r\n\r\n\r\n\r\n", &[]);
        tracker.feed_ordered(
            b"first\r\n$ ",
            &[(0, IsEvent::PromptStart), (9, IsEvent::PromptEnd)],
        );
        assert_eq!(tracker.popup_region(), (3, 4));
        tracker.feed(b"abcdefghijklmnopq", &[]);
        assert_eq!(tracker.state().command, "abcdefghijklmnopq");
        assert_eq!(tracker.popup_region(), (2, 4));
        tracker.feed(b"\x1b[A\r\x1b[4C", &[]);
        assert_eq!(tracker.popup_region(), (2, 4));
    }

    #[test]
    fn scrolling_multiline_prompt_region_is_independent_of_read_boundaries() {
        let wire = b"\x1b]6973;PS\x07abcdefghijklmno\r\n$ \x1b]6973;PE\x07git";
        for split in 0..=wire.len() {
            let mut tracker = TermTracker::new(5, 10);
            tracker.feed(b"\r\n\r\n\r\n\r\n", &[]);
            let mut scanner = crate::ansi::Scanner::new();
            for chunk in [&wire[..split], &wire[split..]] {
                let (bytes, events) = scanner.scan_ordered(chunk);
                tracker.feed_ordered(&bytes, &events);
            }
            assert_eq!(tracker.state().command, "git", "split at {split}");
            assert_eq!(tracker.popup_region(), (2, 4), "split at {split}");
        }
    }

    #[test]
    fn popup_region_remains_bounded_after_resize() {
        let mut tracker = TermTracker::new(12, 10);
        tracker.feed_ordered(
            b"\r\n\r\nfirst\r\n$ abcdefghijklmnopq",
            &[(4, IsEvent::PromptStart), (13, IsEvent::PromptEnd)],
        );
        tracker.resize(4, 20);
        assert_eq!(tracker.popup_region(), (2, 3));
        tracker.resize(12, 20);
        assert_eq!(tracker.popup_region(), (2, 3));
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
