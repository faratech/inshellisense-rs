//! Headless VT tracking + command manager.
//!
//! We feed the (OSC-6973-stripped) PTY output into a vt100 parser so bash +
//! readline can edit their virtual screen line. Combined with prompt-start /
//! prompt-end markers from the shell integration script we can at any moment
//! answer: "what has the user typed on the current command line?".

use crate::ansi::IsEvent;

pub struct TermTracker {
    parser: vt100::Parser,
    #[allow(dead_code)]
    rows: u16,
    cols: u16,
    cwd: String,
    state: CmdState,
}

#[derive(Debug, Clone, Default)]
pub struct CmdState {
    /// Absolute row (baseY + cursorY) where the prompt ended — our "start of
    /// command" anchor. `None` before the first prompt is observed.
    prompt_end_row: Option<usize>,
    prompt_end_col: Option<usize>,
    /// True between PromptStart and PromptEnd.
    in_prompt: bool,
    /// Last snapshot of the command text extracted from the screen.
    pub command: String,
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
        }
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn state(&self) -> &CmdState {
        &self.state
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
        for ev in events {
            match ev {
                IsEvent::PromptStart => {
                    self.state.in_prompt = true;
                    self.state.prompt_end_row = None;
                    self.state.prompt_end_col = None;
                    self.state.command.clear();
                }
                IsEvent::PromptEnd => {
                    self.state.in_prompt = false;
                    let (r, c) = self.parser.screen().cursor_position();
                    self.state.prompt_end_row = Some(r as usize);
                    self.state.prompt_end_col = Some(c as usize);
                }
                IsEvent::Cwd(p) => self.cwd = p.clone(),
            }
        }
        self.refresh_command();
    }

    fn refresh_command(&mut self) {
        let (cursor_row, cursor_col) = self.parser.screen().cursor_position();
        self.state.cursor_row = cursor_row;
        self.state.cursor_col = cursor_col;

        let (Some(pr), Some(pc)) = (self.state.prompt_end_row, self.state.prompt_end_col) else {
            return;
        };

        // Extract text from the prompt-end anchor out to the cursor. We only
        // support single-line commands in the MVP — wrapped multi-line input
        // still works for the visible row but suggestion will be based on the
        // current row only.
        let screen = self.parser.screen();
        let row = cursor_row as usize;
        let mut cmd = String::new();
        if row == pr {
            let line = row_text(screen, row, pc, cursor_col as usize);
            cmd.push_str(&line);
        } else if row > pr {
            // multi-line wrap — concatenate from pr..=row
            let first = row_text(screen, pr, pc, self.cols as usize);
            cmd.push_str(&first);
            for r in (pr + 1)..row {
                cmd.push_str(&row_text(screen, r, 0, self.cols as usize));
            }
            cmd.push_str(&row_text(screen, row, 0, cursor_col as usize));
        }
        // Trim trailing whitespace but keep leading so we know if user is
        // still in the middle of a word.
        let trimmed = cmd.trim_end().to_string();
        self.state.command = trimmed;
    }
}

fn row_text(screen: &vt100::Screen, row: usize, start_col: usize, end_col: usize) -> String {
    let mut s = String::new();
    let max_col = end_col.min(screen.size().1 as usize);
    for col in start_col..max_col {
        if let Some(cell) = screen.cell(row as u16, col as u16) {
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
