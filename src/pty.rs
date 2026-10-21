//! PTY wrapper — spawn the user's shell under portable-pty, proxy I/O
//! to/from the user's real terminal, filter OSC 6973 out, feed a headless
//! vt100 parser, and drive either the ghost-text or interactive popup
//! renderer after each refresh.

use crate::ansi;
use crate::config::{Bindings, UiMode};
use crate::history;
use crate::paths;
use crate::platform::{self, PtyHandle};
use crate::render::{Direction, Renderer};
use crate::shell::Shell;
use crate::spec::Registry;
use crate::spec::model::{Suggestion, SuggestionType};
use crate::suggest::Engine;
use crate::term::TermTracker;
use anyhow::{Context, Result};
use std::io::Write;
use std::thread;

pub fn run_wrapped_shell() -> Result<()> {
    run_wrapped(Shell::Bash, false, None, false)
}

/// Popup state machine — lives only when the active UI is Popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupMode {
    Hidden,
    /// User hit escape; latched until the command text changes.
    Dismissed,
    Visible {
        cursor: usize,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum InputEffect {
    UiOnly,
    ForwardedRedraw,
    Inserted(Vec<u8>),
    Forwarded,
    ForwardedNavigation,
}

/// Expected line-editor state after the whole forwarded burst. Keeping the
/// characters in a Vec makes ordinary append-at-end typeahead amortized O(1).
struct EditState {
    command: Vec<char>,
    cursor: usize,
    text_hash: u64,
}

// Polynomial fingerprint permits constant-time append/pop checkpoints. These
// identify ordinary partial echoes without storing a full line per keystroke.
const ECHO_HASH_BASE: u64 = 0x100000001b3;
const ECHO_HASH_INVERSE: u64 = 0xce965057aff6957b;
const _: () = assert!(ECHO_HASH_BASE.wrapping_mul(ECHO_HASH_INVERSE) == 1);

fn echo_text_hash(chars: impl Iterator<Item = char>) -> u64 {
    chars.fold(0u64, |hash, ch| {
        hash.wrapping_mul(ECHO_HASH_BASE)
            .wrapping_add(ch as u64 + 1)
    })
}

impl EditState {
    fn from_tracker(tracker: &TermTracker) -> Self {
        let command: Vec<_> = tracker.state().command.chars().collect();
        Self {
            text_hash: echo_text_hash(command.iter().copied()),
            command,
            cursor: tracker.state().cursor_offset,
        }
    }

    fn matches(&self, tracker: &TermTracker) -> bool {
        tracker.state().cursor_offset == self.cursor
            && tracker
                .state()
                .command
                .chars()
                .eq(self.command.iter().copied())
    }

    fn apply(
        &mut self,
        mut bytes: &[u8],
        checkpoints: &mut std::collections::HashSet<(u64, usize)>,
    ) -> bool {
        while !bytes.is_empty() {
            // Insert contiguous text together; accepted suggestions can be
            // long, so do not rescan/copy the whole command for each scalar.
            let text_len = bytes
                .iter()
                .position(|b| *b < b' ' || *b == 0x7f)
                .unwrap_or(bytes.len());
            if text_len > 0 {
                let Ok(text) = std::str::from_utf8(&bytes[..text_len]) else {
                    return false;
                };
                if text.chars().any(char::is_control) {
                    return false;
                }
                let suffix_len = self.command.len() - self.cursor;
                let mut painted = 0;
                for ch in text.chars() {
                    if self.cursor == self.command.len() {
                        self.command.push(ch);
                        self.text_hash = self
                            .text_hash
                            .wrapping_mul(ECHO_HASH_BASE)
                            .wrapping_add(ch as u64 + 1);
                    } else {
                        self.command.insert(self.cursor, ch);
                        self.text_hash = echo_text_hash(self.command.iter().copied());
                    }
                    self.cursor += 1;
                    checkpoints.insert((self.text_hash, self.command.len()));
                    // Readline may overwrite at the insertion point before
                    // repainting the retained suffix. That transient screen
                    // is ordinary echo, not an abbreviation/widget rewrite.
                    painted += 1;
                    let overwritten = painted.min(suffix_len);
                    if overwritten > 0 {
                        let paint_hash = echo_text_hash(
                            self.command[..self.cursor]
                                .iter()
                                .chain(self.command[self.cursor + overwritten..].iter())
                                .copied(),
                        );
                        checkpoints.insert((paint_hash, self.command.len() - overwritten));
                    }
                }
                bytes = &bytes[text_len..];
                continue;
            }
            let len = key_len(bytes).max(1).min(bytes.len());
            let key = &bytes[..len];
            match key {
                b"\x08" | b"\x7f" => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        let removed = self.command.remove(self.cursor);
                        if self.cursor == self.command.len() {
                            self.text_hash = self
                                .text_hash
                                .wrapping_sub(removed as u64 + 1)
                                .wrapping_mul(ECHO_HASH_INVERSE);
                        } else {
                            self.text_hash = echo_text_hash(self.command.iter().copied());
                        }
                        let erased = echo_text_hash(
                            self.command[..self.cursor]
                                .iter()
                                .copied()
                                .chain(std::iter::once(' '))
                                .chain(self.command[self.cursor..].iter().copied()),
                        );
                        checkpoints.insert((erased, self.command.len() + 1));
                    }
                }
                b"\x04" | b"\x1b[3~" => {
                    if self.cursor < self.command.len() {
                        self.command.remove(self.cursor);
                        self.text_hash = echo_text_hash(self.command.iter().copied());
                    }
                }
                b"\x02" | b"\x1b[D" | b"\x1bOD" => self.cursor = self.cursor.saturating_sub(1),
                b"\x06" | b"\x1b[C" | b"\x1bOC" => {
                    self.cursor = (self.cursor + 1).min(self.command.len())
                }
                b"\x01" | b"\x1b[H" | b"\x1b[1~" | b"\x1bOH" => self.cursor = 0,
                b"\x05" | b"\x1b[F" | b"\x1b[4~" | b"\x1bOF" => self.cursor = self.command.len(),
                b"\x15" => {
                    self.command.drain(..self.cursor);
                    self.cursor = 0;
                    self.text_hash = echo_text_hash(self.command.iter().copied());
                }
                b"\x0b" => {
                    self.command.truncate(self.cursor);
                    self.text_hash = echo_text_hash(self.command.iter().copied());
                }
                b"\x17" => {
                    let end = self.cursor;
                    while self.cursor > 0 && self.command[self.cursor - 1].is_whitespace() {
                        self.cursor -= 1;
                    }
                    while self.cursor > 0 && !self.command[self.cursor - 1].is_whitespace() {
                        self.cursor -= 1;
                    }
                    self.command.drain(self.cursor..end);
                    self.text_hash = echo_text_hash(self.command.iter().copied());
                }
                b"\x0c" | b"\x1b[200~" | b"\x1b[201~" => {}
                // Shell completion, history search and custom bindings can
                // transform the line arbitrarily. Preserve their observable
                // progress fallback instead of predicting a different shell.
                _ => return false,
            }
            checkpoints.insert((self.text_hash, self.command.len()));
            bytes = &bytes[len..];
        }
        true
    }
}

struct PendingEcho {
    before: u64,
    expected: Option<EditState>,
    allow_bell: bool,
    checkpoints: std::collections::HashSet<(u64, usize)>,
}

fn input_state_signature(tracker: &TermTracker) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    tracker.state().command.hash(&mut hash);
    tracker.state().cursor_offset.hash(&mut hash);
    hash.finish()
}

/// Track request identity separately from text: forwarding a key invalidates
/// both the current results and any response still in flight, even before the
/// shell echoes the edit (#142).
#[derive(Default)]
struct SuggestRequests {
    id: u64,
    context: Option<u64>,
    awaiting_echo: bool,
    echo_from: Option<PendingEcho>,
}

impl SuggestRequests {
    fn invalidate(&mut self) {
        self.id = self.id.wrapping_add(1);
        self.context = None;
    }

    fn forwarded(&mut self, tracker: &TermTracker, bytes: &[u8], allow_bell: bool) {
        self.invalidate();
        let pending = self.echo_from.get_or_insert_with(|| {
            let expected = EditState::from_tracker(tracker);
            let checkpoints = [(expected.text_hash, expected.command.len())]
                .into_iter()
                .collect();
            PendingEcho {
                before: input_state_signature(tracker),
                expected: Some(expected),
                allow_bell,
                checkpoints,
            }
        });
        pending.allow_bell &= allow_bell;
        if let Some(expected) = pending.expected.as_mut()
            && !expected.apply(bytes, &mut pending.checkpoints)
        {
            pending.expected = None;
        }
        self.awaiting_echo = true;
    }

    fn observe(&mut self, tracker: &TermTracker, prompt_started: bool, bell: bool) {
        let Some(pending) = self.echo_from.as_ref() else {
            return;
        };
        let coherent = match &pending.expected {
            Some(expected) => {
                if expected.matches(tracker) {
                    true
                } else {
                    let command = &tracker.state().command;
                    let fingerprint = (echo_text_hash(command.chars()), command.chars().count());
                    // Known intermediate text is still a partial echo. A
                    // different line is a shell transformation (Fish
                    // abbreviation, widget, quoting), so resume from that
                    // observed line rather than wait for impossible text.
                    !pending.checkpoints.contains(&fingerprint)
                }
            }
            None => {
                input_state_signature(tracker) != pending.before || (bell && pending.allow_bell)
            }
        };
        if prompt_started || coherent {
            self.awaiting_echo = false;
            self.echo_from = None;
        }
    }

    fn request(&mut self, typed: &str, cwd: &str) -> Option<u64> {
        if self.awaiting_echo {
            return None;
        }
        let context = suggestion_signature(typed, cwd);
        if self.context == Some(context) {
            return None;
        }
        self.context = Some(context);
        self.id = self.id.wrapping_add(1);
        Some(self.id)
    }
}

struct SuggestResponse {
    signature: u64,
    typed: String,
    ranked: Vec<Suggestion>,
    history_tail: Option<String>,
    partial: Partial,
}

type SuggestRequest = (u64, String, String);

/// Progress that can change the suggestions, even when the terminal is idle.
/// Publishing the registry must schedule the line typed while it was loading.
struct SuggestionTick {
    made_progress: bool,
}

impl SuggestionTick {
    fn new(engine_just_loaded: bool, got_suggestions: bool) -> Self {
        Self {
            made_progress: engine_just_loaded || got_suggestions,
        }
    }

    fn aliases_published(
        &mut self,
        requests: &mut SuggestRequests,
        ranked: &mut Vec<Suggestion>,
        ranked_typed: &mut String,
        history_tail: &mut Option<String>,
        partial: &mut Partial,
        pending_tail: &mut Option<GhostTail>,
    ) {
        // Results computed before the new alias map are stale even when the
        // line and cwd match. Preserve pending shell-echo acknowledgments.
        requests.invalidate();
        ranked.clear();
        ranked_typed.clear();
        *history_tail = None;
        *partial = Partial::default();
        *pending_tail = None;
        self.made_progress = true;
    }

    fn schedule(
        &self,
        requests: &mut SuggestRequests,
        typed: &str,
        cwd: &str,
        sender: &std::sync::mpsc::Sender<SuggestRequest>,
    ) -> Result<(), std::sync::mpsc::SendError<SuggestRequest>> {
        if self.made_progress
            && let Some(id) = requests.request(typed, cwd)
        {
            sender.send((id, typed.to_string(), cwd.to_string()))?;
        }
        Ok(())
    }
}

impl SuggestResponse {
    fn apply(
        self,
        request_signature: u64,
        ranked: &mut Vec<Suggestion>,
        ranked_typed: &mut String,
        history_tail: &mut Option<String>,
        partial: &mut Partial,
    ) -> bool {
        if self.signature != request_signature {
            return false;
        }
        *ranked = self.ranked;
        *ranked_typed = self.typed;
        *history_tail = self.history_tail;
        *partial = self.partial;
        true
    }
}

/// Spawn a wrapped shell.
///
/// `ui_override` takes precedence over the `ui` field in the loaded
/// config. This is how `is start --ui popup` reaches the renderer.
///
/// `test` makes the child shell render a deterministic `> ` prompt, which is
/// what `is start -T` and the parity harness rely on.
pub fn run_wrapped(
    shell: Shell,
    login: bool,
    ui_override: Option<UiMode>,
    test: bool,
) -> Result<()> {
    // Make sure the vendored shell integration scripts are on disk.
    crate::resources::unpack()?;

    let shell_path = find_shell_binary(shell)?;
    let shell_dir = paths::shell_dir().context("no HOME directory")?;
    let zsh_dotdir = paths::zsh_dotdir().context("no HOME directory")?;
    let target = shell.spawn_target(&shell_dir, &zsh_dotdir, login);
    let cfg = crate::config::load();
    let effective_ui = ui_override.unwrap_or(cfg.ui);

    let (cols, rows) = platform::term_size().unwrap_or((80, 24));

    // Build the environment for the child shell. `spawn_env` is the single
    // source of truth for the session/login/testing markers that the shell
    // integration scripts look for.
    let test = test || crate::env::test_active();
    let mut child_env: Vec<(String, String)> = crate::env::spawn_env(login, test)
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    child_env.push(("TERM".into(), "xterm-256color".into()));
    for (k, v) in &target.env {
        child_env.push((k.clone(), v.clone()));
    }
    // Only the wrapped Fish receives this policy. Explicit false is needed
    // for Popup to override a flag inherited from an outer Ghost session.
    if shell == Shell::Fish {
        child_env.push((
            "INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS".into(),
            if matches!(effective_ui, UiMode::Ghost | UiMode::Hybrid) {
                "1"
            } else {
                "0"
            }
            .into(),
        ));
    }
    if let Ok(home) = std::env::var("HOME") {
        child_env.push(("HOME".into(), home));
    }
    if let Ok(path) = std::env::var("PATH") {
        child_env.push(("PATH".into(), path));
    }

    // Build argv: [shell_path, ...target.args].
    let mut argv = vec![shell_path.clone()];
    argv.extend(target.args.iter().cloned());

    #[cfg(unix)]
    let mut pty = platform::UnixPty::spawn(&shell_path, &argv, &child_env, rows, cols)
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    #[cfg(windows)]
    let mut pty = platform::WindowsPty::spawn(&shell_path, &argv, &child_env, rows, cols)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    if !platform::enable_raw_mode() {
        // Without a terminal in raw mode we can neither observe keystrokes
        // nor keep the display coherent; refuse rather than misbehave (#61).
        anyhow::bail!("stdin is not an interactive terminal; `is start` must run inside a shell");
    }
    platform::install_signal_handlers();

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        platform::disable_raw_mode();
        prev_hook(info);
    }));

    // Clear the host terminal on startup — matches upstream's
    // `writeOutput(ansi.clearTerminal)` in ui-root.ts. Without this,
    // any leftover output from before `is start` was invoked stays
    // visible and interferes with the popup's cursor-relative draws
    // (the first popup ends up rendering inside whatever stale text
    // happened to be on screen). `\x1b[2J` erases the visible screen,
    // `\x1b[3J` erases the scrollback, `\x1b[H` moves the cursor to
    // the home position.
    // On Unix, clear the screen so leftover output doesn't interfere
    // with popup rendering. On Windows, ConPTY handles its own screen
    // so clearing the parent terminal causes a flash.
    #[cfg(unix)]
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[2J\x1b[3J\x1b[H").ok();
        out.flush().ok();
    }

    // Background engine load — ~500ms of zstd + JSON parse off the
    // critical path so the shell prompt appears instantly.
    let engine: std::sync::Arc<std::sync::RwLock<Option<Engine>>> =
        std::sync::Arc::new(std::sync::RwLock::new(None));
    let (aliases_tx, aliases_rx) = std::sync::mpsc::channel::<()>();
    {
        let engine = engine.clone();
        let alias_shell = shell;
        let use_aliases = cfg.use_aliases;
        thread::spawn(move || {
            let registry = Registry::new_with_defaults();
            // Seed the snapshot from the WRAPPED shell, not the parent's.
            let hist = history::load_for(alias_shell);
            let mut built = Engine::new(registry, hist);
            built.set_shell(alias_shell);
            // Publish first, then decorate with aliases: loading them spawns
            // an interactive shell that sources the user's rc files, and a
            // slow or blocking rc file must not leave `is start` without any
            // suggestions at all. Commands complete right away; shortcut
            // aliases arrive shortly after.
            if let Ok(mut slot) = engine.write() {
                *slot = Some(built);
            }
            if use_aliases && load_engine_aliases(&engine, || crate::alias::load(alias_shell)) {
                let _ = aliases_tx.send(());
            }
        });
    }

    // Suggestion worker. Generators shell out (`git branch`, `docker ps`, …)
    // with a 5-second default timeout, and `suggest_blob` used to run on this
    // thread — so a slow generator froze keystroke handling for as long as it
    // took. Compute suggestions off the event loop and deliver them by channel.
    let (req_tx, req_rx) = std::sync::mpsc::channel::<SuggestRequest>();
    let (res_tx, res_rx) = std::sync::mpsc::channel::<SuggestResponse>();
    {
        let engine = engine.clone();
        thread::spawn(move || {
            while let Ok(mut request) = req_rx.recv() {
                // Only the newest request matters; drop anything queued behind
                // it so a burst of keystrokes doesn't run N generators.
                while let Ok(newer) = req_rx.try_recv() {
                    request = newer;
                }
                let (sig, typed, cwd) = request;
                let Ok(guard) = engine.read() else { continue };
                let Some(engine_ref) = guard.as_ref() else {
                    let _ = res_tx.send(SuggestResponse {
                        signature: sig,
                        typed,
                        ranked: Vec::new(),
                        history_tail: None,
                        partial: Partial::default(),
                    });
                    continue;
                };
                let (ranked, active_token) = engine_ref.suggest_blob_with_token(&typed, &cwd);
                let partial = active_token
                    .map(|token| Partial {
                        text: token.token,
                        consumed: token.token_length,
                    })
                    .unwrap_or_default();
                // The ghost for a ranked suggestion is built on the event
                // loop from the suggestion itself, so only the history
                // fallback is needed from the engine. (`Engine::suggest`
                // would also run `suggest_blob`, and every generator, again.)
                let history_tail = engine_ref.history_tail(&typed);
                if res_tx
                    .send(SuggestResponse {
                        signature: sig,
                        typed,
                        ranked,
                        history_tail,
                        partial,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut tracker = TermTracker::new(rows, cols);
    let mut ansi_scanner = ansi::Scanner::new();
    // Popup / ghost dispatch: CLI --ui flag (ui_override) beats config
    // file, which beats the built-in default (Ghost).
    let has_popup = matches!(effective_ui, UiMode::Popup | UiMode::Hybrid);
    let has_ghost = matches!(effective_ui, UiMode::Ghost | UiMode::Hybrid);
    let bindings: Bindings = cfg.bindings.clone();
    let max_popup_rows = cfg.max_suggestions.max(1) as usize + 1 /* desc line */ + 4 /* desc overflow headroom */;
    let mut render_cfg = cfg.clone();
    render_cfg.ui = effective_ui;
    let mut renderer = Renderer::with_config(
        &render_cfg,
        crate::render::popup::IconSet::from_config(cfg.use_nerd_font),
    );

    // Shared suggestion state.
    let mut pending_tail: Option<GhostTail> = None; // ghost mode
    let mut ranked: Vec<Suggestion> = Vec::new();
    let mut popup_mode = PopupMode::Hidden;
    let mut last_cmd_signature = String::new();
    let mut last_stdin_was_history = false;
    // Set to true after the user submits a command (Enter/Ctrl-C) so the
    // next redraw is suppressed until bash emits the next PromptStart.
    // Without this we race bash's echo: we clear the popup, forward `\r`,
    // then the same tick's redraw re-emits the popup because
    // `state.command` still points at the old text — and by the time
    // bash scrolls the screen, the new popup top-border is stranded
    // wherever the cursor happened to be.
    let mut submitting = false;
    // Inside a bracketed paste (ESC[200~ .. ESC[201~) stdin bytes are
    // literal text, never UI keys (#115). Persists across reads.
    let mut in_bracketed_paste = false;
    // Tracks the background registry load so the first tick after it lands
    // can redraw the suggestion for text already on the line.
    let mut engine_was_ready = false;
    // Signature of the text the outstanding request was made for, and the text
    // the suggestions currently in `ranked` were computed from.
    let mut requests = SuggestRequests::default();
    let mut ranked_typed = String::new();
    let mut history_tail: Option<String> = None;
    let mut ranked_partial = Partial::default();

    let mut pty_buf = [0u8; 4096];
    let mut stdin_buf = [0u8; 1024];

    // The wrapped shell's exit status is ours to report: `is start` then
    // `exit 42` must exit 42, not 0. Stays 0 if we leave the loop for any
    // reason other than the child exiting.
    let mut child_exit_code = 0;

    loop {
        if let Some(code) = pty.try_wait() {
            child_exit_code = code;
            // Drain remaining PTY output before shutting down. The child's
            // final output (including console-mode-reset sequences) must
            // reach the parent terminal — but the drain must be BOUNDED:
            // the master only reaches EOF once every process holding the
            // slave open has exited, so a `sleep 300 &` left running by the
            // user used to freeze `is` (raw mode still on, keystrokes
            // dropped) until that job died (#52). With the master fd in
            // O_NONBLOCK mode, read_pty returns EAGAIN instead of blocking;
            // we drain already queued output but impose a total deadline,
            // including while data flows. A continuously writing descendant
            // (`yes & exit`) otherwise keeps us here forever (#139).
            drain_exited_pty(
                &pty,
                &mut ansi_scanner,
                &mut out,
                std::time::Duration::from_millis(500),
            );
            out.flush().ok();
            break;
        }

        // SIGWINCH — one ioctl per wakeup.
        if let Some((new_cols, new_rows)) = platform::term_size()
            && new_cols > 0
            && new_rows > 0
            && (new_rows != tracker.rows() || new_cols != tracker.cols())
        {
            tracker.resize(new_rows, new_cols);
            pty.resize(new_rows, new_cols);
            renderer.clear(&mut out).ok();
        }

        // Block until data arrives on PTY or stdin (0% CPU idle).
        let (pty_ready, stdin_ready) = pty.poll(50);

        // The registry loads on a background thread. Without this check a user
        // who finished typing before the load completed saw no suggestion at
        // all until the next keystroke or byte of shell output, because a bare
        // poll timeout `continue`d without ever redrawing.
        // Readiness is monotonic after publication. Never wait on this lock:
        // alias publication can be queued behind a worker's slow generator,
        // and a pending writer can also block subsequent blocking readers.
        let engine_ready =
            engine_was_ready || engine.try_read().map(|g| g.is_some()).unwrap_or(false);
        let engine_just_loaded = engine_ready && !engine_was_ready;
        engine_was_ready = engine_ready;

        let mut tick = SuggestionTick::new(engine_just_loaded, false);
        // The one-shot alias load changes completions for unchanged text.
        // Consume publication before worker responses so old IDs are rejected.
        if aliases_rx.try_recv().is_ok() {
            tick.aliases_published(
                &mut requests,
                &mut ranked,
                &mut ranked_typed,
                &mut history_tail,
                &mut ranked_partial,
                &mut pending_tail,
            );
            renderer.clear(&mut out).ok();
            if popup_mode != PopupMode::Dismissed {
                popup_mode = PopupMode::Hidden;
            }
        }

        // Pick up anything the suggestion worker finished while we were idle.
        let mut got_suggestions = false;
        while let Ok(response) = res_rx.try_recv() {
            got_suggestions |= response.apply(
                requests.id,
                &mut ranked,
                &mut ranked_typed,
                &mut history_tail,
                &mut ranked_partial,
            );
        }

        tick.made_progress |= got_suggestions;
        if !pty_ready && !stdin_ready && !tick.made_progress {
            continue;
        }

        // Read PTY output if ready.
        if pty_ready {
            // Bounded drain (#114): continuous output (`yes`) keeps the
            // PTY always-ready, and an unbounded loop here never returns
            // to the outer poll — stdin (Ctrl-C included) starves. The
            // outer loop re-polls immediately, so capping chunks per
            // tick costs no throughput.
            let mut chunks: u32 = 0;
            loop {
                chunks += 1;
                if chunks > 128 {
                    break;
                }
                let n = pty.read_pty(&mut pty_buf);
                if n < 1 {
                    break;
                }
                let bytes = &pty_buf[..n as usize];
                let (clean, osc_events) = ansi_scanner.scan_ordered(bytes);
                if !clean.is_empty() {
                    renderer.clear(&mut out).ok();
                }
                out.write_all(&clean).ok();
                out.flush().ok();
                // A new prompt means the previous command may have been
                // appended to the shell's history file. Reload it so ghost
                // text can suggest what the user just ran.
                if osc_events
                    .iter()
                    .any(|(_, e)| matches!(e, ansi::IsEvent::PromptStart))
                {
                    // `try_write`, never `write`: the suggestion worker may be
                    // holding the read lock across a slow generator, and this
                    // refresh is best-effort.
                    if let Ok(mut guard) = engine.try_write()
                        && let Some(engine) = guard.as_mut()
                    {
                        engine.refresh_history();
                    }
                }
                tracker.feed_ordered(&clean, &osc_events);
                requests.observe(
                    &tracker,
                    osc_events
                        .iter()
                        .any(|(_, e)| matches!(e, ansi::IsEvent::PromptStart)),
                    clean.contains(&ansi::BEL),
                );
                pending_tail = None;
                tick.made_progress = true;
                // Check if more data without blocking.
                let (more, _) = pty.poll(0);
                if !more {
                    break;
                }
            }
        }

        // Read stdin if ready.
        if stdin_ready {
            let n = pty.read_stdin(&mut stdin_buf);
            // Fallback for ConPTY: OSC 6973 markers are stripped, so the
            // prompt anchor is never set via events. Infer it from the cursor
            // position the moment the user starts typing. This must happen
            // *after* a key actually arrives: on Windows the console signals
            // readiness for focus/mouse/resize records too, and anchoring on
            // one of those latched a stale position that could never be reset.
            if n > 0 && !tracker.has_prompt_anchor() {
                tracker.set_fallback_anchor();
            }
            if n > 0 {
                let buf = stdin_buf[..n as usize].to_vec();
                // One `read` can carry several keys. Dispatch them one at a
                // time so each is matched against the bindings on its own.
                for bytes in split_keys(&buf) {
                    // Bracketed paste: forward verbatim until the end
                    // marker — no binding matching, no submitting latch,
                    // no history tracking (#115).
                    if in_bracketed_paste || bytes == b"\x1b[200~" {
                        if bytes == b"\x1b[201~" {
                            in_bracketed_paste = false;
                        } else if bytes == b"\x1b[200~" {
                            in_bracketed_paste = true;
                        }
                        renderer.clear(&mut out).ok();
                        pty.pty_write(bytes);
                        pending_tail = None;
                        ranked.clear();
                        ranked_typed.clear();
                        history_tail = None;
                        ranked_partial = Partial::default();
                        popup_mode = PopupMode::Hidden;
                        requests.forwarded(&tracker, bytes, false);
                        continue;
                    }
                    // Track Up/Down for history-hide behavior (upstream:
                    // suggestionManager.ts:172-174).
                    last_stdin_was_history = bytes == b"\x1b[A"
                        || bytes == b"\x1b[B"
                        || bytes == b"\x1bOA"
                        || bytes == b"\x1bOB";
                    let effect = handle_stdin(
                        bytes,
                        has_ghost,
                        has_popup,
                        &mut pending_tail,
                        &mut popup_mode,
                        &ranked,
                        &ranked_typed,
                        &bindings,
                        shell,
                        &tracker,
                        &mut renderer,
                        &mut out,
                        &pty,
                        &mut submitting,
                        &ranked_partial,
                    );
                    let forwarded = match &effect {
                        InputEffect::Inserted(insert) => Some(insert.as_slice()),
                        InputEffect::Forwarded | InputEffect::ForwardedNavigation => Some(bytes),
                        InputEffect::UiOnly | InputEffect::ForwardedRedraw => None,
                    };
                    if let Some(forwarded) = forwarded {
                        pending_tail = None;
                        ranked.clear();
                        ranked_typed.clear();
                        history_tail = None;
                        ranked_partial = Partial::default();
                        let allow_bell = effect == InputEffect::ForwardedNavigation
                            || (effect == InputEffect::Forwarded
                                && bytes.len() == 1
                                && bytes[0].is_ascii_control()
                                && !matches!(bytes[0], b'\r' | b'\n' | 0x03));
                        requests.forwarded(&tracker, forwarded, allow_bell);
                    }
                }
                // A modeled no-op (End at EOL, or an empty paste) may never
                // echo. Its final state already matches the tracker, so it
                // is safe to recover without waiting for unrelated output.
                requests.observe(&tracker, false, false);
                tick.made_progress = true;
            }
        }

        if tick.made_progress {
            let state = tracker.state().clone();
            let cwd = tracker.cwd().to_string();
            if requests.awaiting_echo || in_bracketed_paste {
                last_cmd_signature = state.command.clone();
                out.flush().ok();
                continue;
            }

            // If the command was just submitted (Enter/Ctrl-C), the
            // tracker's `state.command` is still whatever it was before
            // bash echoed the newline. Don't draw anything until the
            // next PromptStart resets `state.command` to empty.
            if submitting {
                if state.command.is_empty() {
                    // PromptStart has fired — we're back at a fresh
                    // prompt. Release the latch.
                    submitting = false;
                    ranked.clear();
                    pending_tail = None;
                    popup_mode = PopupMode::Hidden;
                    renderer.clear(&mut out).ok();
                    last_cmd_signature.clear();
                } else {
                    // Still post-Enter but pre-PromptStart. Make sure
                    // nothing is drawn this tick.
                    renderer.clear(&mut out).ok();
                }
            } else if state.command.is_empty() {
                ranked.clear();
                pending_tail = None;
                popup_mode = PopupMode::Hidden;
                renderer.clear(&mut out).ok();
                last_cmd_signature.clear();
            } else {
                // Re-arm a dismissed popup on any edit to the line,
                // UNLESS the change came from Up/Down history navigation
                // (upstream: suggestionManager.ts:172-174). We detect
                // history by checking if the last stdin was an arrow key.
                if popup_mode == PopupMode::Dismissed
                    && state.command != last_cmd_signature
                    && !last_stdin_was_history
                {
                    popup_mode = PopupMode::Hidden;
                }

                // The engine loads on a background thread; until it lands
                // there is nothing to suggest. Typed keystrokes still reach
                // the shell normally.
                if !engine_ready {
                    last_cmd_signature = state.command.clone();
                    continue;
                }

                // Complete from the text before the cursor, not the whole
                // line: with the cursor mid-word the suffix isn't typed yet
                // from the completion engine's point of view.
                let typed = tracker.command_before_cursor();
                if tick.schedule(&mut requests, typed, &cwd, &req_tx).is_err() {
                    break;
                }
                // Results for the text on screen have not arrived yet. Leave
                // the last coherent popup up rather than blanking the screen
                // on every keystroke; the accept path checks `ranked_typed`.
                if ranked_typed != typed {
                    last_cmd_signature = state.command.clone();
                    continue;
                }

                // Ghost text is only SAFE to draw when the cursor is at
                // the very end of the command line. If the user has
                // arrow-keyed into the middle of their text, writing
                // grey ghost chars at the current cursor would
                // overwrite their typed characters. Pop-ups are still
                // safe (they draw on a separate line below/above), but
                // ghost must be suppressed.
                let cursor_at_end = tracker.cursor_at_command_end();

                let active_cursor = match popup_mode {
                    PopupMode::Visible { cursor } if !ranked.is_empty() => {
                        cursor.min(ranked.len() - 1)
                    }
                    _ => 0,
                };
                let ghost = if has_ghost && cursor_at_end {
                    ranked
                        .get(active_cursor)
                        .and_then(|s| suggestion_ghost(s, typed, &ranked_partial, Some(shell)))
                        .or_else(|| history_tail.as_deref().and_then(history_ghost))
                } else {
                    None
                };
                let tail = ghost.as_ref().map(|g| g.display.clone());
                pending_tail = ghost;

                if has_popup && popup_mode != PopupMode::Dismissed && !ranked.is_empty() {
                    let (popup_direction, available_rows, gap_rows) =
                        popup_placement(&tracker, max_popup_rows);
                    let cursor_col = tracker.state().cursor_col;
                    let term_cols = tracker.cols();
                    renderer
                        .draw_popup_interactive_with_gap(
                            &mut out,
                            tail.as_deref(),
                            &ranked,
                            active_cursor,
                            popup_direction,
                            cursor_col,
                            term_cols,
                            available_rows,
                            gap_rows,
                        )
                        .ok();
                    popup_mode = if renderer.popup_visible() {
                        PopupMode::Visible {
                            cursor: active_cursor,
                        }
                    } else {
                        PopupMode::Hidden
                    };
                } else if has_popup && ranked.is_empty() {
                    popup_mode = PopupMode::Hidden;
                    renderer.clear(&mut out).ok();
                } else if !has_popup {
                    // Pure Ghost mode.
                    let ghost_cells =
                        tracker.cols().saturating_sub(tracker.state().cursor_col) as usize;
                    renderer
                        .draw(&mut out, tail.as_deref(), &ranked, ghost_cells)
                        .ok();
                }

                last_cmd_signature = state.command.clone();
            }
        }
        // No else-sleep needed — recv_timeout at the top of the loop
        // parks the thread when there's no activity.
    }

    renderer.clear(&mut out).ok();
    // RIS (Reset to Initial State) — resets the entire terminal.
    // This is how upstream inshellisense ensures the parent terminal
    // recovers cleanly regardless of what ConPTY did to the state.
    out.write_all(b"\x1bc").ok();
    out.flush().ok();
    drop(out);
    platform::disable_raw_mode();
    #[cfg(unix)]
    pty.close();
    let _ = std::panic::take_hook();
    // Exit with the wrapped shell's status. On Windows this also lets the OS
    // tear down ConPTY — calling ClosePseudoConsole explicitly deadlocks and
    // white-flashes in Windows Terminal.
    std::process::exit(child_exit_code);
}

fn popup_placement(tracker: &TermTracker, max_rows: usize) -> (Direction, usize, u16) {
    let (first, last) = tracker.popup_region();
    let rows_above = first as usize;
    let rows_below = tracker.rows().saturating_sub(last.saturating_add(1)) as usize;
    let cursor = tracker.state().cursor_row;
    // Keep the whole prompt and command protected, including command rows
    // below a cursor moved into the middle of a wrapped line (#143).
    if rows_below > max_rows {
        (Direction::Below, rows_below, last.saturating_sub(cursor))
    } else if rows_above >= max_rows || rows_above > rows_below {
        (Direction::Above, rows_above, cursor.saturating_sub(first))
    } else {
        (Direction::Below, rows_below, last.saturating_sub(cursor))
    }
}

fn drain_exited_pty(
    pty: &dyn PtyHandle,
    scanner: &mut ansi::Scanner,
    out: &mut impl Write,
    budget: std::time::Duration,
) {
    let started = std::time::Instant::now();
    let mut buf = [0; 4096];
    let mut quiet_ticks = 0;
    while started.elapsed() < budget {
        let n = pty.read_pty(&mut buf);
        if n > 0 {
            quiet_ticks = 0;
            let (clean, _) = scanner.scan(&buf[..n as usize]);
            out.write_all(&clean).ok();
            continue;
        }
        quiet_ticks += 1;
        if quiet_ticks > 25 {
            break;
        }
        let remaining = budget.saturating_sub(started.elapsed());
        let wait_ms = remaining.as_millis().min(20) as i32;
        if wait_ms == 0 {
            break;
        }
        let (ready, _) = pty.poll(wait_ms);
        if !ready && quiet_ticks > 2 {
            break;
        }
    }
}

/// Alias discovery sources arbitrary shell startup files. Keep it outside the
/// engine lock so the event loop and suggestion worker can use the published
/// engine while the subprocess is running (#119).
fn load_engine_aliases(
    engine: &std::sync::RwLock<Option<Engine>>,
    load: impl FnOnce() -> std::collections::HashMap<String, String>,
) -> bool {
    let aliases = load();
    if aliases.is_empty() {
        return false;
    }
    if let Ok(mut guard) = engine.write()
        && let Some(published) = guard.as_mut()
    {
        published.set_aliases(aliases);
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn handle_stdin(
    bytes: &[u8],
    has_ghost: bool,
    has_popup: bool,
    pending_tail: &mut Option<GhostTail>,
    popup_mode: &mut PopupMode,
    ranked: &[Suggestion],
    // The text `ranked` was computed from. Suggestions are produced off the
    // event loop, so they can lag the line by a tick.
    ranked_typed: &str,
    bindings: &Bindings,
    // The shell being wrapped — decides how an accepted filesystem
    // suggestion is quoted (POSIX splice vs PowerShell/cmd rules).
    shell: Shell,
    tracker: &TermTracker,
    renderer: &mut Renderer,
    out: &mut impl Write,
    pty: &dyn PtyHandle,
    submitting: &mut bool,
    ranked_partial: &Partial,
) -> InputEffect {
    // Ghost-accept (right / End / Ctrl-E).
    if has_ghost {
        let ghost_accept: &[&[u8]] = &[b"\x1b[C", b"\x1b[F", b"\x05"];
        if ghost_accept.contains(&bytes)
            && let Some(ghost) = pending_tail.take()
        {
            renderer.clear(out).ok();
            // The accept bytes, never the displayed text: they carry the
            // quoting and the backspaces that rewrite the token.
            pty.pty_write(&ghost.insert);
            *popup_mode = PopupMode::Hidden;
            return InputEffect::Inserted(ghost.insert);
        }
    }
    // Popup key interception.
    if has_popup
        && let PopupMode::Visible { cursor } = *popup_mode
        && !ranked.is_empty()
    {
        if bindings.next_suggestion.matches(bytes) {
            *popup_mode = PopupMode::Visible {
                cursor: (cursor + 1).min(ranked.len() - 1),
            };
            return InputEffect::UiOnly;
        }
        if bindings.previous_suggestion.matches(bytes) {
            *popup_mode = PopupMode::Visible {
                cursor: cursor.saturating_sub(1),
            };
            return InputEffect::UiOnly;
        }
        if bindings.accept_suggestion.matches(bytes) {
            let typed = tracker.command_before_cursor();
            // Never accept a suggestion computed for different text —
            // its replacement span would not match the current line.
            if typed != ranked_typed {
                renderer.clear(out).ok();
                *popup_mode = PopupMode::Hidden;
                pty.pty_write(bytes);
                *pending_tail = None;
                return InputEffect::Forwarded;
            }
            let selected = &ranked[cursor.min(ranked.len() - 1)];
            let repl = replacement_tail(selected, typed, ranked_partial, Some(shell));
            renderer.clear(out).ok();
            // A `{cursor}` suggestion places the caret inside the
            // inserted text, so no separating space belongs after it.
            let wants_space =
                !repl.had_marker && !matches!(selected.suggestion_type, SuggestionType::Folder);
            let caret = repl.caret_move();
            let mut insert = repl.tail.into_bytes();
            if wants_space {
                insert.push(b' ');
            }
            insert.extend(caret);
            *popup_mode = PopupMode::Hidden;
            *pending_tail = None;
            return if insert.is_empty() {
                InputEffect::UiOnly
            } else {
                pty.pty_write(&insert);
                InputEffect::Inserted(insert)
            };
        }
        if bindings.dismiss_suggestions.matches(bytes) {
            renderer.clear(out).ok();
            *popup_mode = PopupMode::Dismissed;
            // Don't return — forward the key to the shell
            // (upstream returns false here).
        }
        // Return/Ctrl-C: clear and forward (upstream:
        // suggestionManager.ts:203-205).
        else if bytes.iter().any(|&b| b == b'\r' || b == 0x03) {
            renderer.clear(out).ok();
            *popup_mode = PopupMode::Hidden;
            // Fall through to forward path.
        }
        // Anything else: close popup, fall through.
        else {
            renderer.clear(out).ok();
            *popup_mode = PopupMode::Hidden;
        }
    }
    // Default forward path.
    if is_cursor_navigation(bytes) {
        if renderer.ghost_visible() {
            renderer.clear_ghost(out).ok();
        }
    } else {
        renderer.clear(out).ok();
    }
    let forwarded_cursor_navigation = is_cursor_navigation(bytes);
    *pending_tail = None;
    if bytes.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0x03) {
        *submitting = true;
        *popup_mode = PopupMode::Hidden;
    }
    pty.pty_write(bytes);
    if forwarded_cursor_navigation {
        InputEffect::ForwardedNavigation
    } else if bytes == b"\x0c" {
        InputEffect::ForwardedRedraw
    } else {
        InputEffect::Forwarded
    }
}

/// Is this stdin chunk a pure cursor-navigation key that doesn't
/// modify the command text? These keys should NOT trigger a clear of
/// the ghost renderer because clearing emits `\x1b[K` (erase line
/// right) which wipes any typed characters ahead of the cursor.
fn is_cursor_navigation(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        // Arrow keys (CSI form and SS3 form)
        b"\x1b[A" | b"\x1b[B" | b"\x1b[C" | b"\x1b[D"
        | b"\x1bOA" | b"\x1bOB" | b"\x1bOC" | b"\x1bOD"
        // Home / End (CSI, VT, SS3 variants)
        | b"\x1b[H" | b"\x1b[1~" | b"\x1bOH" | b"\x1b[F" | b"\x1b[4~" | b"\x1bOF"
        // Page up / down
        | b"\x1b[5~" | b"\x1b[6~"
        // Ctrl-A (home), Ctrl-B (back-char), Ctrl-E (end),
        // Ctrl-F (forward-char). Ctrl-E is a ghost-accept key when a
        // ghost exists; the accept path handles that before this fallback.
        | b"\x01" | b"\x02" | b"\x05" | b"\x06"
        // Meta-B / Meta-F (word navigation in bash)
        | b"\x1bb" | b"\x1bf"
    )
}

/// Split a raw stdin buffer into individual key presses.
///
/// Terminal input is a byte stream, not one key per `read`. Every binding
/// check compares the *whole* buffer, so two arrows delivered together
/// (`\x1b[B\x1b[B`) matched nothing and fell through to "close the popup".
/// Windows makes this the common case: `read_stdin` deliberately drains many
/// `KEY_EVENT` records into one buffer.
fn split_keys(buf: &[u8]) -> Vec<&[u8]> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let len = key_len(&buf[i..]).max(1);
        keys.push(&buf[i..(i + len).min(buf.len())]);
        i += len;
    }
    keys
}

/// Length of the single key press at the front of `buf`.
fn key_len(buf: &[u8]) -> usize {
    match buf {
        // CSI: ESC [ ... <final byte in 0x40..=0x7e>
        [0x1b, b'[', rest @ ..] => match rest.iter().position(|b| (0x40..=0x7e).contains(b)) {
            Some(idx) => 2 + idx + 1,
            // Truncated sequence: consume what we have.
            None => buf.len(),
        },
        // SS3: ESC O <one byte>
        [0x1b, b'O', _, ..] => 3,
        // Alt-<char>: ESC followed by a printable byte.
        [0x1b, b, ..] if b.is_ascii_graphic() => 2,
        // Lone ESC.
        [0x1b, ..] => 1,
        // Otherwise one UTF-8 scalar.
        [b, ..] => utf8_len(*b),
        [] => 0,
    }
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // Stray continuation byte — consume it alone rather than stalling.
        _ => 1,
    }
}

/// Identity of a suggestion request: the text before the cursor plus the cwd.
fn suggestion_signature(typed: &str, cwd: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    typed.hash(&mut hasher);
    cwd.hash(&mut hasher);
    // 0 is the "nothing requested yet" sentinel.
    hasher.finish() | 1
}

/// Marks where the caret should land after an `insert_value` is inserted.
const CURSOR_MARKER: &str = "{cursor}";

/// The in-progress token at the end of the line, and how much of the raw line
/// it occupies.
///
/// Splitting the raw line on whitespace was wrong in two ways. For
/// `cargo build --target=wa` it reported the partial as `--target=wa`, so
/// accepting a suggestion backspaced over `--target=` and destroyed the flag.
/// For `cat "my fi` it reported `fi` while the suggestion was `my file.txt`,
/// producing `cat "my my file.txt`. The tokenizer already records both the
/// token's value and how many characters it spans on the line (`token_length`
/// counts the quotes), so use that.
#[derive(Debug, Clone, PartialEq, Default)]
struct Partial {
    /// The value being completed — what a suggestion name is compared against.
    text: String,
    /// Characters of the raw line the token occupies, quotes included.
    consumed: usize,
}

#[cfg(test)]
fn current_partial(command: &str) -> Partial {
    match crate::spec::parse_command(command).pop() {
        Some(token) if !token.complete => Partial {
            text: token.token,
            consumed: token.token_length,
        },
        _ => Partial::default(),
    }
}

/// What to send the shell to accept a suggestion.
#[derive(Debug, Clone, PartialEq, Default)]
struct Replacement {
    /// Bytes fed to the shell's line editor.
    tail: String,
    /// Characters the caret must move back over afterwards, to honor a
    /// `{cursor}` marker.
    cursor_left: usize,
    /// The suggestion carried a `{cursor}`, so no trailing space is appended.
    had_marker: bool,
}

impl Replacement {
    /// Left-arrows, not backspaces: in a shell line editor `\x08` is
    /// backward-delete-char, which would eat the text just inserted.
    fn caret_move(&self) -> Vec<u8> {
        b"\x1b[D".repeat(self.cursor_left)
    }
}

/// Split an `insert_value` at its `{cursor}` marker into the text to insert
/// and how many characters follow the caret. `-app '{cursor}'` inserts
/// `-app ''` and leaves the caret between the quotes. Without this the marker
/// was inserted literally — 31 bundled specs carry one.
fn split_cursor_marker(raw: &str) -> (String, usize, bool) {
    let Some(idx) = raw.find(CURSOR_MARKER) else {
        return (raw.to_string(), 0, false);
    };
    let before = &raw[..idx];
    // Any further markers are literal noise; drop them.
    let after = raw[idx + CURSOR_MARKER.len()..].replace(CURSOR_MARKER, "");
    let trailing = after.chars().count();
    (format!("{before}{after}"), trailing, true)
}

/// Characters that must never reach the shell's line editor from a
/// suggestion. Suggestion names can come from the filesystem, so a file named
/// `foo<newline>id` would otherwise submit a second command, and an embedded
/// ESC would be read as the start of a key sequence.
fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Does this word need quoting to survive the shell's word splitting?
fn needs_quoting(s: &str) -> bool {
    const SPECIAL: &[char] = &[
        ' ', '\t', '"', '\'', '\\', '$', '`', '|', '&', ';', '<', '>', '(', ')', '*', '?', '[',
        ']', '{', '}', '!', '#', '~',
    ];
    s.chars().any(|c| SPECIAL.contains(&c))
}

/// Wrap a filesystem name or generator value in the quoting syntax of the
/// shell being wrapped so spaces and metacharacters insert literally. Applied
/// to File/Folder suggestions, whose names come from the filesystem, and to
/// `external` values (generator output, project files); an `insert_value`
/// or a spec's own suggestion name is shell text authored by the spec and
/// must be inserted verbatim.
///
/// The quote style differs per shell, and using POSIX rules everywhere made
/// the insertion un-runnable elsewhere: cmd.exe does not treat `'…'` as a
/// quote at all (`cd 'Program Files'` fails), and PowerShell does not treat
/// `\` as an escape outside/inside quotes, so the POSIX splice inserted a
/// stray backslash.
fn quote_for_shell(s: &str, shell: Option<Shell>) -> String {
    match shell {
        // PowerShell escapes a quote inside a single-quoted literal by
        // doubling it; every other character stays literal. Its tokenizer
        // also treats the typographic single quotes U+2018/2019/201A/201B as
        // quote characters, so each of those must be doubled too: an
        // undoubled U+2019 ends the literal and the rest of the name runs as
        // commands.
        Some(Shell::Pwsh | Shell::Powershell) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('\'');
            for c in s.chars() {
                out.push(c);
                if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
                    out.push(c);
                }
            }
            out.push('\'');
            out
        }
        // fish honors only `\\` and `\'` as escapes inside single quotes, so
        // the POSIX splice below mis-parses a name holding a backslash next to
        // a quote (`a\'; id #` closes the literal early and `id` runs).
        Some(Shell::Fish) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        // xonsh strings follow Python rules: `\'` escapes the quote and `\\`
        // is an escaped backslash, so backslashes are doubled first (a name
        // ending `\'` would otherwise cancel the quote's escape).
        Some(Shell::Xonsh) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Some(Shell::Nu) => {
            if s.contains('\'') {
                // Nushell's single-quoted strings have no way to represent an
                // embedded quote, so fall back to double quotes, where `\\`
                // and `\"` are escapes and the backslashes of Windows paths
                // stay intact.
                format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                format!("'{}'", s)
            }
        }
        // cmd.exe only ever honors double quotes, and has no escape character
        // for an embedded one; doubling is the closest approximation.
        #[cfg(windows)]
        Some(Shell::Cmd) => format!("\"{}\"", s.replace('"', "\"\"")),
        // bash / zsh / fish: splice an embedded quote out of and back into the
        // quoted word ('o'\''brien').
        _ => format!("'{}'", s.replace('\'', "'\\''")),
    }
}

/// The ghost suggestion: what is painted after the cursor, and what accepting
/// it (Right / End / Ctrl-E) writes to the shell. The two differ whenever the
/// accepted text cannot be shown inline — it rewrites the token (a quoted
/// filename) or moves the caret (`{cursor}`).
#[derive(Debug, Clone, PartialEq)]
struct GhostTail {
    /// Painted after the cursor. Never written to the shell.
    display: String,
    /// Written to the shell on accept.
    insert: Vec<u8>,
}

/// The ghost for a ranked suggestion. Accepting it writes the same
/// `Replacement` popup accept does — quoted, control characters stripped —
/// minus popup accept's separating space.
///
/// Ghost accept used to write the engine's raw tail whenever that replacement
/// started with backspaces or carried a `{cursor}` marker. That is exactly the
/// case for every filename that needs quoting, so `cat a` + Right inserted
/// `a b.txt` as two words, and a name's control characters (a CR submits the
/// line) reached the line editor (#86).
fn suggestion_ghost(
    suggestion: &Suggestion,
    line: &str,
    partial: &Partial,
    shell: Option<Shell>,
) -> Option<GhostTail> {
    let repl = replacement_tail(suggestion, line, partial, shell);
    let mut insert = repl.tail.clone().into_bytes();
    insert.extend(repl.caret_move());
    let display = if repl.tail.starts_with('\x08') || repl.had_marker {
        // Not displayable inline: show the rest of the name instead.
        // Case-insensitive like the engine's filtering (#105): an
        // exact-case strip_prefix lost the ghost for `git CH`.
        crate::suggest::ci_tail(&strip_control_chars(&suggestion.name), &partial.text)?
    } else {
        repl.tail
    };
    if display.is_empty() {
        return None;
    }
    Some(GhostTail { display, insert })
}

/// The ghost for a history completion: the user's own command text, inserted
/// verbatim apart from control characters.
fn history_ghost(tail: &str) -> Option<GhostTail> {
    let tail = strip_control_chars(tail);
    if tail.is_empty() {
        return None;
    }
    Some(GhostTail {
        insert: tail.clone().into_bytes(),
        display: tail,
    })
}

/// Given an accepted suggestion and the line so far, the bytes to write into
/// the shell so the line ends with `insert_value` (or `name`).
fn replacement_tail(
    suggestion: &Suggestion,
    line: &str,
    partial: &Partial,
    shell: Option<Shell>,
) -> Replacement {
    let (target, cursor_left, had_marker) = match suggestion.insert_value.as_deref() {
        Some(raw) => split_cursor_marker(&strip_control_chars(raw)),
        None => {
            let name = strip_control_chars(&suggestion.name);
            // A git ref may legally contain `;`, `|`, `$`, `(` or a
            // backtick, and a `package.json` script name anything at all:
            // unquoted, they become separators or substitutions when the
            // line runs.
            let quote = (suggestion.external
                || matches!(
                    suggestion.suggestion_type,
                    SuggestionType::File | SuggestionType::Folder
                ))
                && needs_quoting(&name);
            let rendered = if quote {
                quote_for_shell(&name, shell)
            } else {
                name
            };
            (rendered, 0, false)
        }
    };

    // The exact characters the token occupies on the line, quotes included.
    let chars: Vec<char> = line.chars().collect();
    let start = chars.len().saturating_sub(partial.consumed);
    let raw: String = chars[start..].iter().collect();

    let tail = match target.strip_prefix(raw.as_str()) {
        // The typed text is a literal prefix of the insertion: just append.
        Some(rest) => rest.to_string(),
        // Otherwise erase the token and rewrite it whole. This is the path a
        // quoted or `--opt=`-split partial takes.
        None => {
            let mut s = "\x08".repeat(partial.consumed);
            s.push_str(&target);
            s
        }
    };

    Replacement {
        tail,
        cursor_left,
        had_marker,
    }
}

/// Resolve the binary path for a shell, with platform-specific logic.
///
/// On Windows, `bash.exe` on PATH is often WSL's bash
/// (`C:\Windows\System32\bash.exe`), NOT a native shell. Prefer known
/// Git Bash / MSYS2 install paths via `find_git_bash()`.
fn find_shell_binary(shell: Shell) -> Result<String> {
    #[cfg(windows)]
    {
        if shell == Shell::Bash {
            // Try known Git Bash / MSYS2 install locations first.
            if let Some(p) = platform::find_git_bash() {
                return Ok(p);
            }
            // Fall through to PATH, but skip WSL's bash.exe.
            if let Some(p) = platform::find_on_path("bash") {
                let lower = p.to_lowercase();
                if !lower.contains("windows\\system32") && !lower.contains("windows/system32") {
                    return Ok(p);
                }
            }
            anyhow::bail!(
                "bash not found — install Git for Windows or MSYS2, \
                 or use `is start --shell pwsh`"
            );
        }
    }
    platform::find_on_path(shell.as_str())
        .ok_or_else(|| anyhow::anyhow!("shell not found on PATH: {}", shell.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn slow_alias_load_leaves_published_engine_readable() {
        use std::sync::{Arc, RwLock, mpsc};

        let engine = Arc::new(RwLock::new(Some(Engine::new(
            Registry::default(),
            vec!["git status".into()],
        ))));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let loader_engine = Arc::clone(&engine);
        let loader = thread::spawn(move || {
            load_engine_aliases(&loader_engine, || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                [("audit_alias".into(), "git status".into())]
                    .into_iter()
                    .collect()
            });
        });

        // Synchronize with the blocked subprocess without a timing-dependent
        // sleep. A held write lock would make the event loop's read block.
        started_rx.recv().unwrap();
        let history_while_loading = engine
            .try_read()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|e| e.history_tail("git s")));
        // Always release/join before asserting so a regression cannot strand
        // the test's loader thread.
        release_tx.send(()).unwrap();
        loader.join().unwrap();
        assert_eq!(history_while_loading.as_deref(), Some("tatus"));

        let guard = engine.read().unwrap();
        let suggestions = guard.as_ref().unwrap().suggest_blob("audit_a", "/");
        assert!(suggestions.iter().any(|s| s.name == "audit_alias"));
    }

    #[test]
    fn alias_load_reports_only_nonempty_maps_published_to_an_engine() {
        let aliases = || [("g".into(), "git".into())].into_iter().collect();
        assert!(!load_engine_aliases(&std::sync::RwLock::new(None), aliases));
        let engine = std::sync::RwLock::new(Some(Engine::new(Registry::default(), vec![])));
        assert!(!load_engine_aliases(&engine, Default::default));
        assert!(load_engine_aliases(&engine, aliases));
        assert!(
            engine
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .suggest_blob("g", "/")
                .iter()
                .any(|suggestion| suggestion.name == "g")
        );
    }

    #[derive(Default)]
    struct FakePty {
        writes: RefCell<Vec<u8>>,
        queued_output: RefCell<std::collections::VecDeque<Vec<u8>>>,
        repeat_output: Cell<bool>,
        reads: Cell<usize>,
    }

    impl PtyHandle for FakePty {
        fn output_fd(&self) -> crate::platform::RawDescriptor {
            #[cfg(unix)]
            {
                0
            }
            #[cfg(windows)]
            {
                std::ptr::null_mut()
            }
        }

        fn stdin_fd(&self) -> crate::platform::RawDescriptor {
            self.output_fd()
        }

        fn pty_write(&self, data: &[u8]) {
            self.writes.borrow_mut().extend_from_slice(data);
        }

        fn resize(&self, _rows: u16, _cols: u16) {}

        fn try_wait(&mut self) -> Option<i32> {
            None
        }

        fn poll(&self, _timeout_ms: i32) -> (bool, bool) {
            (
                self.repeat_output.get() || !self.queued_output.borrow().is_empty(),
                false,
            )
        }

        fn read_pty(&self, buf: &mut [u8]) -> isize {
            self.reads.set(self.reads.get() + 1);
            if let Some(chunk) = self.queued_output.borrow_mut().pop_front() {
                buf[..chunk.len()].copy_from_slice(&chunk);
                chunk.len() as isize
            } else if self.repeat_output.get() {
                buf.fill(b'x');
                buf.len() as isize
            } else {
                0
            }
        }

        fn read_stdin(&self, _buf: &mut [u8]) -> isize {
            -1
        }

        fn close(&mut self) {}
    }

    #[test]
    fn exit_drain_has_a_total_deadline_during_continuous_output() {
        let pty = FakePty {
            repeat_output: Cell::new(true),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        drain_exited_pty(
            &pty,
            &mut ansi::Scanner::new(),
            &mut std::io::sink(),
            std::time::Duration::from_millis(30),
        );
        assert!(pty.reads.get() > 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn exit_drain_forwards_all_queued_output_without_a_chunk_cap() {
        let chunks = 1024;
        let pty = FakePty {
            queued_output: RefCell::new((0..chunks).map(|_| vec![b'x'; 4096]).collect()),
            ..Default::default()
        };
        let mut out = Vec::new();
        drain_exited_pty(
            &pty,
            &mut ansi::Scanner::new(),
            &mut out,
            std::time::Duration::from_secs(10),
        );
        assert!(pty.queued_output.borrow().is_empty());
        assert_eq!(out, vec![b'x'; chunks * 4096]);
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_exit_drain_finishes_with_a_writing_descendant_and_preserves_status() {
        let dir = crate::test_support::unique_temp_dir("exit-drain");
        let pid_path = dir.join("background.pid");
        let bash = platform::find_on_path("bash").unwrap();
        let argv = vec![
            bash.clone(),
            "--noprofile".into(),
            "--norc".into(),
            "-i".into(),
            "-c".into(),
            "trap '' HUP; yes x & printf '%s' \"$!\" > \"$INSH_TEST_CHILD_PID\"; exit 42".into(),
        ];
        let env = vec![(
            "INSH_TEST_CHILD_PID".into(),
            pid_path.to_str().unwrap().into(),
        )];
        let mut pty = platform::UnixPty::spawn(&bash, &argv, &env, 24, 80).unwrap();
        let started = std::time::Instant::now();
        let mut buf = [0; 4096];
        let mut relayed_bytes = 0;
        let status = loop {
            // Relay while the shell exits, like the main loop; a busy
            // descendant can otherwise fill the PTY before bash's exit echo.
            let n = pty.read_pty(&mut buf);
            if n > 0 {
                relayed_bytes += n as usize;
            }
            if let Some(status) = pty.try_wait() {
                break Some(status);
            }
            if started.elapsed() > std::time::Duration::from_secs(5) {
                break None;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        };
        let background_pid = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.parse::<i32>().ok());
        let writer_alive_before =
            background_pid.is_some_and(|pid| unsafe { libc::kill(pid, 0) == 0 });
        let drain_started = std::time::Instant::now();
        let mut queued_output = Vec::new();
        drain_exited_pty(
            &pty,
            &mut ansi::Scanner::new(),
            &mut queued_output,
            std::time::Duration::from_millis(100),
        );
        let drained_in = drain_started.elapsed();
        let writer_alive_after =
            background_pid.is_some_and(|pid| unsafe { libc::kill(pid, 0) == 0 });
        pty.close();
        if let Some(pid) = background_pid {
            // The PTY session ended; reap the fixture's background workload
            // even if a failed assertion is about to unwind the test.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(status, Some(42));
        assert!(background_pid.is_some());
        assert!(writer_alive_before && writer_alive_after);
        assert!(relayed_bytes + queued_output.len() > 0);
        // The real writer may be descheduled long enough to hit the quiet
        // exit first. The deterministic always-ready fake above proves the
        // total deadline; this test proves lifecycle, output and exit status.
        assert!(drained_in < std::time::Duration::from_secs(2));
    }

    /// Accept `s` against the whole line, deriving the partial the way the
    /// event loop does. Defaults to no shell context (POSIX quoting).
    fn accept(s: &Suggestion, line: &str) -> Replacement {
        accept_as(s, line, None)
    }

    /// Same, with an explicit shell so per-shell quoting can be exercised.
    fn accept_as(s: &Suggestion, line: &str, shell: Option<Shell>) -> Replacement {
        let partial = current_partial(line);
        replacement_tail(s, line, &partial, shell)
    }

    fn sug(name: &str) -> Suggestion {
        Suggestion {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn registry_publication_schedules_an_unchanged_line_without_terminal_activity() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"> ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        tracker.feed(b"git ch", &[]);
        let mut requests = SuggestRequests::default();
        let (sender, worker) = std::sync::mpsc::channel();

        // The line has already echoed while the registry is still loading.
        // No subsequent PTY bytes, stdin, or worker responses wake the UI.
        SuggestionTick::new(false, false)
            .schedule(
                &mut requests,
                tracker.command_before_cursor(),
                "/repo",
                &sender,
            )
            .unwrap();
        assert!(worker.try_recv().is_err());

        // The first idle tick that observes publication must send that exact
        // line to the worker, without requiring an edit or a Ctrl-L redraw.
        SuggestionTick::new(true, false)
            .schedule(
                &mut requests,
                tracker.command_before_cursor(),
                "/repo",
                &sender,
            )
            .unwrap();
        let (id, typed, cwd) = worker
            .try_recv()
            .expect("publication must request suggestions");
        assert_eq!((typed.as_str(), cwd.as_str()), ("git ch", "/repo"));
        assert_eq!(id, requests.id);

        SuggestionTick::new(false, false)
            .schedule(
                &mut requests,
                tracker.command_before_cursor(),
                "/repo",
                &sender,
            )
            .unwrap();
        assert!(
            worker.try_recv().is_err(),
            "idle ticks must not duplicate work"
        );
    }

    #[test]
    fn alias_publication_replaces_same_line_requests_without_acknowledging_pending_edits() {
        for pending_edit in [false, true] {
            let mut tracker = TermTracker::new(24, 80);
            tracker.feed(
                b"> ",
                &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
            );
            tracker.feed(b"g ch", &[]);
            let mut requests = SuggestRequests::default();
            let (sender, worker) = std::sync::mpsc::channel();
            SuggestionTick::new(true, false)
                .schedule(&mut requests, "g ch", "/repo", &sender)
                .unwrap();
            let (old_id, old_typed, _) = worker.try_recv().unwrap();
            let old_response = SuggestResponse {
                signature: old_id,
                typed: old_typed.clone(),
                ranked: vec![sug("old")],
                history_tail: Some("old".into()),
                partial: current_partial(&old_typed),
            };
            let mut ranked = vec![sug("old")];
            let mut ranked_typed = old_typed.clone();
            let mut history_tail = Some("old".into());
            let mut partial = current_partial(&old_typed);
            let mut pending_tail = history_ghost("old");
            if pending_edit {
                requests.forwarded(&tracker, b"e", false);
            }
            let previous_id = requests.id;
            let mut tick = SuggestionTick::new(false, false);
            tick.aliases_published(
                &mut requests,
                &mut ranked,
                &mut ranked_typed,
                &mut history_tail,
                &mut partial,
                &mut pending_tail,
            );
            assert_ne!(requests.id, previous_id);
            assert!(ranked.is_empty());
            assert!(ranked_typed.is_empty());
            assert!(history_tail.is_none());
            assert_eq!(partial, Partial::default());
            assert!(pending_tail.is_none());
            assert!(!old_response.apply(
                requests.id,
                &mut ranked,
                &mut ranked_typed,
                &mut history_tail,
                &mut partial,
            ));
            tick.schedule(&mut requests, "g ch", "/repo", &sender)
                .unwrap();
            if pending_edit {
                assert!(requests.awaiting_echo);
                assert!(
                    worker.try_recv().is_err(),
                    "publication cannot acknowledge an edit"
                );
                requests.observe(&tracker, false, false);
                assert!(requests.awaiting_echo);
                tracker.feed(b"e", &[]);
                requests.observe(&tracker, false, false);
                assert!(!requests.awaiting_echo);
                tick.schedule(
                    &mut requests,
                    tracker.command_before_cursor(),
                    "/repo",
                    &sender,
                )
                .unwrap();
            }
            let (new_id, new_typed, cwd) =
                worker.try_recv().expect("new alias map must be queried");
            assert_ne!(new_id, old_id);
            assert_eq!(new_typed, if pending_edit { "g che" } else { "g ch" });
            assert_eq!(cwd, "/repo");
            tick.schedule(&mut requests, &new_typed, &cwd, &sender)
                .unwrap();
            assert!(
                worker.try_recv().is_err(),
                "publication schedules only once"
            );
        }
    }

    #[test]
    fn response_carries_resolved_literal_argument_span_through_both_accept_paths() {
        let typed = "cmd --name --flag=va";
        // The resolver knows this entire word is the required argument of
        // --name. A context-free parse splits it after '=' instead.
        assert_eq!(current_partial(typed).text, "va");
        let resolved = Partial {
            text: "--flag=va".into(),
            consumed: 9,
        };
        let response = SuggestResponse {
            signature: 1,
            typed: typed.into(),
            ranked: vec![sug("--flag=value")],
            history_tail: None,
            partial: resolved.clone(),
        };
        let mut ranked = Vec::new();
        let mut ranked_typed = String::new();
        let mut history_tail = None;
        let mut partial = Partial::default();
        assert!(response.apply(
            1,
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial,
        ));
        assert_eq!(partial, resolved);

        let stale = SuggestResponse {
            signature: 0,
            typed: "old".into(),
            ranked: vec![sug("old")],
            history_tail: Some("old".into()),
            partial: current_partial("old"),
        };
        assert!(!stale.apply(
            1,
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial,
        ));
        assert_eq!(
            partial, resolved,
            "a stale response cannot replace the span"
        );
        assert_eq!(ranked_typed, typed);

        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"$ ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        tracker.feed(typed.as_bytes(), &[]);
        for ui in [UiMode::Ghost, UiMode::Popup] {
            let pty = FakePty::default();
            let mut pending_tail = suggestion_ghost(&ranked[0], typed, &partial, Some(Shell::Bash));
            assert_eq!(pending_tail.as_ref().unwrap().display, "lue");
            let mut popup_mode = PopupMode::Visible { cursor: 0 };
            let effect = handle_stdin(
                if ui == UiMode::Ghost {
                    b"\x1b[C"
                } else {
                    b"\t"
                },
                ui == UiMode::Ghost,
                ui == UiMode::Popup,
                &mut pending_tail,
                &mut popup_mode,
                &ranked,
                &ranked_typed,
                &Bindings::default(),
                Shell::Bash,
                &tracker,
                &mut Renderer::new(ui, 5, crate::render::popup::IconSet::Emoji),
                &mut Vec::new(),
                &pty,
                &mut false,
                &partial,
            );
            let expected = if ui == UiMode::Ghost {
                b"lue".as_slice()
            } else {
                b"lue "
            };
            assert_eq!(&*pty.writes.borrow(), expected);
            assert_eq!(effect, InputEffect::Inserted(expected.to_vec()));
        }
    }

    #[test]
    fn response_received_before_new_echo_keeps_its_original_typed_text() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"$ ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        tracker.feed(b"git ", &[]);
        let request_signature = suggestion_signature("git ", "/");
        let response = SuggestResponse {
            signature: request_signature,
            typed: "git ".into(),
            ranked: vec![sug("add")],
            history_tail: None,
            partial: Partial::default(),
        };
        let mut ranked = Vec::new();
        let mut ranked_typed = String::new();
        let mut history_tail = None;
        let mut partial = Partial::default();
        assert!(response.apply(
            request_signature,
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial
        ));
        // The same event-loop tick then consumes a newer PTY echo. The old
        // response must not be relabeled with this text or accepted for it.
        tracker.feed(b"s", &[]);
        assert_ne!(ranked_typed, tracker.command_before_cursor());
        let pty = FakePty::default();
        let mut popup_mode = PopupMode::Visible { cursor: 0 };
        handle_stdin(
            b"\t",
            false,
            true,
            &mut None,
            &mut popup_mode,
            &ranked,
            &ranked_typed,
            &Bindings::default(),
            Shell::Bash,
            &tracker,
            &mut Renderer::new(UiMode::Popup, 5, crate::render::popup::IconSet::Emoji),
            &mut Vec::new(),
            &pty,
            &mut false,
            &partial,
        );
        assert_eq!(&*pty.writes.borrow(), b"\t", "stale add must not replace s");
        assert_eq!(popup_mode, PopupMode::Hidden);

        let stale = SuggestResponse {
            signature: request_signature,
            typed: "git ".into(),
            ranked: vec![sug("branch")],
            history_tail: Some("branch".into()),
            partial: Partial::default(),
        };
        assert!(!stale.apply(
            suggestion_signature("git s", "/"),
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial
        ));
        assert_eq!(ranked[0].name, "add");
        assert_eq!(ranked_typed, "git ");
        assert!(history_tail.is_none());
    }

    #[test]
    fn typeahead_clears_ghost_and_popup_acceptance_before_shell_echo() {
        for (ui, keys) in [
            (UiMode::Ghost, b"s\x1b[C".as_slice()),
            (UiMode::Popup, b"s\t".as_slice()),
            (UiMode::Hybrid, b"s\x1b[C".as_slice()),
        ] {
            let mut tracker = TermTracker::new(24, 80);
            tracker.feed(
                b"$ ",
                &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
            );
            tracker.feed(b"git ", &[]);
            let has_ghost = matches!(ui, UiMode::Ghost | UiMode::Hybrid);
            let has_popup = matches!(ui, UiMode::Popup | UiMode::Hybrid);
            let mut pending_tail = if has_ghost {
                history_ghost("status")
            } else {
                None
            };
            let mut popup_mode = if has_popup {
                PopupMode::Visible { cursor: 0 }
            } else {
                PopupMode::Hidden
            };
            let pty = FakePty::default();
            let mut renderer = Renderer::new(ui, 5, crate::render::popup::IconSet::Emoji);
            let ranked = vec![sug("status")];
            for key in split_keys(keys) {
                let effect = handle_stdin(
                    key,
                    has_ghost,
                    has_popup,
                    &mut pending_tail,
                    &mut popup_mode,
                    &ranked,
                    "git ",
                    &Bindings::default(),
                    Shell::Bash,
                    &tracker,
                    &mut renderer,
                    &mut Vec::new(),
                    &pty,
                    &mut false,
                    &current_partial("git "),
                );
                assert_ne!(effect, InputEffect::UiOnly);
                assert!(pending_tail.is_none());
            }
            assert_eq!(
                pty.writes.borrow().as_slice(),
                keys,
                "stale status was accepted in {ui:?}"
            );
            assert_eq!(popup_mode, PopupMode::Hidden);
        }
    }

    #[test]
    fn canceled_responses_cannot_restore_acceptance_before_echo_or_after_a_new_request() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"$ ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        tracker.feed(b"git ", &[]);
        let mut requests = SuggestRequests::default();
        let old_id = requests.request("git ", "/").unwrap();
        assert!(requests.request("git ", "/").is_none());
        requests.forwarded(&tracker, b"s", false);
        assert!(requests.awaiting_echo);
        assert!(requests.request("git ", "/").is_none());
        // A background repaint and unrelated beep are not the edit's echo.
        tracker.feed(b"\x1b7\x1b[10;1Hasync output\x1b8", &[]);
        requests.observe(&tracker, false, false);
        assert!(requests.awaiting_echo);
        tracker.feed(b"\x07", &[]);
        requests.observe(&tracker, false, true);
        assert!(requests.awaiting_echo);
        let response = |id, typed: &str| SuggestResponse {
            signature: id,
            typed: typed.into(),
            ranked: vec![sug("status")],
            history_tail: None,
            partial: current_partial(typed),
        };
        let mut ranked = Vec::new();
        let mut ranked_typed = String::new();
        let mut history_tail = None;
        let mut partial = Partial::default();
        assert!(!response(old_id, "git ").apply(
            requests.id,
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial
        ));
        assert!(ranked.is_empty());

        // Only the actual edit advances the tracked command and permits a
        // fresh request. The old identity remains canceled after recovery.
        tracker.feed(b"s", &[]);
        requests.observe(&tracker, false, false);
        assert!(!requests.awaiting_echo);
        let new_id = requests
            .request(tracker.command_before_cursor(), "/")
            .unwrap();
        assert_ne!(new_id, old_id);
        assert!(!response(old_id, "git ").apply(
            requests.id,
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial
        ));
        assert!(response(new_id, "git s").apply(
            requests.id,
            &mut ranked,
            &mut ranked_typed,
            &mut history_tail,
            &mut partial
        ));
        let mut pending_tail =
            suggestion_ghost(&ranked[0], &ranked_typed, &partial, Some(Shell::Bash));
        let pty = FakePty::default();
        let effect = handle_stdin(
            b"\x1b[C",
            true,
            false,
            &mut pending_tail,
            &mut PopupMode::Hidden,
            &ranked,
            &ranked_typed,
            &Bindings::default(),
            Shell::Bash,
            &tracker,
            &mut Renderer::new(UiMode::Ghost, 5, crate::render::popup::IconSet::Emoji),
            &mut Vec::new(),
            &pty,
            &mut false,
            &partial,
        );
        assert_eq!(effect, InputEffect::Inserted(b"tatus".to_vec()));
        assert_eq!(pty.writes.borrow().as_slice(), b"tatus");

        // A no-op navigation key may report a bell without changing text.
        requests.forwarded(&tracker, b"\x1b[C", true);
        requests.observe(&tracker, false, true);
        assert!(requests.request("git s", "/").is_some());
    }

    #[test]
    fn redraw_only_input_preserves_outstanding_suggestion_request() {
        let tracker = TermTracker::new(24, 80);
        let mut requests = SuggestRequests::default();
        let id = requests.request("git s", "/").unwrap();
        let pty = FakePty::default();
        let effect = handle_stdin(
            b"\x0c",
            true,
            false,
            &mut None,
            &mut PopupMode::Hidden,
            &[],
            "git s",
            &Bindings::default(),
            Shell::Bash,
            &tracker,
            &mut Renderer::new(UiMode::Ghost, 5, crate::render::popup::IconSet::Emoji),
            &mut Vec::new(),
            &pty,
            &mut false,
            &current_partial("git s"),
        );
        assert_eq!(effect, InputEffect::ForwardedRedraw);
        assert_eq!(pty.writes.borrow().as_slice(), b"\x0c");
        assert!(!requests.awaiting_echo);
        assert_eq!(requests.id, id);
        assert!(requests.request("git s", "/").is_none());
    }

    #[test]
    fn partial_echo_of_a_forwarded_burst_cannot_reactivate_suggestions() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"$ ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        tracker.feed(b"git ", &[]);
        let mut requests = SuggestRequests::default();
        let old = requests.request("git ", "/").unwrap();
        requests.forwarded(&tracker, b"s", false);
        requests.forwarded(&tracker, b"t", false);
        tracker.feed(b"s", &[]);
        requests.observe(&tracker, false, false);
        assert!(requests.awaiting_echo);
        assert!(requests.request("git s", "/").is_none());
        let mut ranked = Vec::new();
        let mut typed = String::new();
        let mut partial = Partial::default();
        let mut tail = None;
        assert!(
            !SuggestResponse {
                signature: old,
                typed: "git ".into(),
                ranked: vec![sug("status")],
                history_tail: None,
                partial: Partial::default(),
            }
            .apply(
                requests.id,
                &mut ranked,
                &mut typed,
                &mut tail,
                &mut partial
            )
        );
        assert!(ranked.is_empty());

        tracker.feed(b"t", &[]);
        requests.observe(&tracker, false, false);
        assert!(!requests.awaiting_echo);
        let fresh = requests
            .request(tracker.command_before_cursor(), "/")
            .unwrap();
        assert!(
            SuggestResponse {
                signature: fresh,
                typed: "git st".into(),
                ranked: vec![sug("status")],
                history_tail: None,
                partial: current_partial("git st"),
            }
            .apply(
                requests.id,
                &mut ranked,
                &mut typed,
                &mut tail,
                &mut partial
            )
        );
        assert_eq!(
            suggestion_ghost(&ranked[0], &typed, &partial, Some(Shell::Bash))
                .unwrap()
                .insert,
            b"atus"
        );
    }

    #[test]
    fn expected_echo_tracks_navigation_and_inserted_text_as_one_burst() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"$ ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        tracker.feed(b"git s", &[]);
        let mut requests = SuggestRequests::default();
        requests.forwarded(&tracker, b"\x1b[D", true);
        requests.forwarded(&tracker, b"t", false);
        tracker.feed(b"\x1b[D", &[]);
        requests.observe(&tracker, false, false);
        assert!(requests.awaiting_echo);
        // Readline paints the inserted character and retained suffix, then
        // moves back over the suffix to restore the insertion caret.
        tracker.feed(b"t", &[]);
        requests.observe(&tracker, false, false);
        assert!(requests.awaiting_echo);
        tracker.feed(b"s\x08", &[]);
        requests.observe(&tracker, false, false);
        assert_eq!(tracker.state().command, "git ts");
        assert!(!requests.awaiting_echo);

        requests.forwarded(&tracker, b"\x15echo", false);
        tracker.feed(b"\r$ echos\x1b[K\x08", &[]);
        requests.observe(&tracker, false, false);
        assert_eq!(tracker.state().command, "echos");
        assert!(!requests.awaiting_echo);
    }

    #[test]
    fn shell_abbreviation_rewrite_recovers_and_accepts_normal_following_completion() {
        let mut tracker = TermTracker::new(24, 80);
        tracker.feed(
            b"$ ",
            &[ansi::IsEvent::PromptStart, ansi::IsEvent::PromptEnd],
        );
        let mut requests = SuggestRequests::default();
        requests.forwarded(&tracker, b"zzabbr ", false);
        tracker.feed(b"zzabbr", &[]);
        requests.observe(&tracker, false, false);
        assert!(
            requests.awaiting_echo,
            "ordinary partial echo must stay blocked"
        );
        // Fish's abbreviation expansion erases the literal word and paints
        // a different command. Waiting for the literal zzabbr would stall.
        tracker.feed(b"\r$ git \x1b[K", &[]);
        requests.observe(&tracker, false, false);
        assert!(!requests.awaiting_echo);
        assert!(requests.request("git ", "/").is_some());
        requests.forwarded(&tracker, b"ch", false);
        tracker.feed(b"ch", &[]);
        requests.observe(&tracker, false, false);
        assert!(!requests.awaiting_echo);
        let id = requests.request("git ch", "/").unwrap();
        let mut ranked = Vec::new();
        let mut typed = String::new();
        let mut partial = Partial::default();
        assert!(
            SuggestResponse {
                signature: id,
                typed: "git ch".into(),
                ranked: vec![sug("checkout")],
                history_tail: None,
                partial: current_partial("git ch"),
            }
            .apply(
                requests.id,
                &mut ranked,
                &mut typed,
                &mut None,
                &mut partial
            )
        );
        let mut pending = suggestion_ghost(&ranked[0], &typed, &partial, Some(Shell::Fish));
        let pty = FakePty::default();
        handle_stdin(
            b"\x1b[C",
            true,
            false,
            &mut pending,
            &mut PopupMode::Hidden,
            &ranked,
            &typed,
            &Bindings::default(),
            Shell::Fish,
            &tracker,
            &mut Renderer::new(UiMode::Ghost, 5, crate::render::popup::IconSet::Emoji),
            &mut Vec::new(),
            &pty,
            &mut false,
            &partial,
        );
        assert_eq!(pty.writes.borrow().as_slice(), b"eckout");
    }

    #[test]
    fn popup_placement_excludes_every_prompt_and_wrapped_command_row() {
        let mut tracker = TermTracker::new(12, 10);
        tracker.feed_ordered(b"\r\n\r\n", &[]);
        tracker.feed_ordered(
            b"first\r\n$ abcdefghijklmnopq",
            &[
                (0, ansi::IsEvent::PromptStart),
                (9, ansi::IsEvent::PromptEnd),
            ],
        );
        tracker.feed_ordered(b"\x1b[A\r\x1b[4C", &[]);
        assert_eq!(tracker.popup_region(), (2, 4));
        assert_eq!(popup_placement(&tracker, 8), (Direction::Below, 7, 1));

        // Near the bottom, above placement must skip all earlier prompt rows,
        // not merely take the raw cursor row as free space.
        let mut tracker = TermTracker::new(5, 10);
        tracker.feed_ordered(b"\r\n\r\n\r\n", &[]);
        tracker.feed_ordered(
            b"first\r\n$ git",
            &[
                (0, ansi::IsEvent::PromptStart),
                (9, ansi::IsEvent::PromptEnd),
            ],
        );
        assert_eq!(tracker.popup_region(), (3, 4));
        assert_eq!(popup_placement(&tracker, 8), (Direction::Above, 3, 1));
    }

    #[test]
    fn partial_trailing_word() {
        assert_eq!(current_partial("git ch").text, "ch");
        assert_eq!(current_partial("git").text, "git");
        assert_eq!(current_partial("git ").text, "");
        assert_eq!(current_partial("git ").consumed, 0);
    }

    /// The tokenizer splits `--opt=value`, so only `value` is replaced.
    /// Splitting the raw line on whitespace made the partial `--target=wa`,
    /// and accepting a suggestion backspaced the flag away.
    #[test]
    fn partial_stops_at_option_value_separator() {
        let p = current_partial("cargo build --target=wa");
        assert_eq!(p.text, "wa");
        assert_eq!(p.consumed, 2);
    }

    /// `token_length` counts the opening quote, so the whole quoted token is
    /// replaced rather than just its last whitespace-delimited chunk.
    #[test]
    fn partial_spans_the_opening_quote() {
        let p = current_partial(r#"cat "my fi"#);
        assert_eq!(p.text, "my fi");
        assert_eq!(p.consumed, 6);
    }

    #[test]
    fn replacement_extends_partial() {
        assert_eq!(accept(&sug("checkout"), "git ch").tail, "eckout");
    }

    #[test]
    fn replacement_backspaces_when_prefix_mismatches() {
        let out = accept(&sug("pull"), "git ch").tail;
        assert!(out.starts_with("\x08\x08"));
        assert!(out.ends_with("pull"));
    }

    /// `{cursor}` marks where the caret lands; it must never be inserted
    /// literally. 31 bundled specs carry one.
    #[test]
    fn cursor_marker_is_stripped_and_moves_the_caret() {
        let s = Suggestion {
            name: "foo".into(),
            insert_value: Some("foo={cursor}".into()),
            ..Default::default()
        };
        let repl = accept(&s, "git fo");
        assert_eq!(repl.tail, "o=");
        assert_eq!(repl.cursor_left, 0);
        assert!(repl.had_marker);
    }

    /// A marker in the middle leaves the caret inside the inserted text.
    /// `-app '{cursor}'` must land the caret between the quotes.
    #[test]
    fn cursor_marker_in_the_middle_walks_the_caret_back() {
        let s = Suggestion {
            name: "-app".into(),
            insert_value: Some("-app '{cursor}'".into()),
            ..Default::default()
        };
        // `-a` is a literal prefix of `-app ''`, so only the tail is written.
        let repl = accept(&s, "cmd -a");
        assert_eq!(repl.tail, "pp ''");
        assert_eq!(repl.cursor_left, 1);
        // Left-arrow, not backspace: backspace would delete the quote.
        assert_eq!(repl.caret_move(), b"\x1b[D");
        assert!(repl.had_marker);

        // With no typed partial the whole value is inserted.
        let repl = accept(&s, "cmd ");
        assert_eq!(repl.tail, "-app ''");
        assert_eq!(repl.cursor_left, 1);
    }

    #[test]
    fn cursor_marker_before_text_walks_back_over_it() {
        let s = Suggestion {
            name: "b".into(),
            insert_value: Some("{cursor}b".into()),
            ..Default::default()
        };
        let repl = accept(&s, "dd ");
        assert_eq!(repl.tail, "b");
        assert_eq!(repl.cursor_left, 1);
        assert_eq!(repl.caret_move(), b"\x1b[D");
    }

    /// Suggestion names can come from the filesystem. A newline would submit
    /// a second command; ESC would start a key sequence.
    #[test]
    fn control_characters_never_reach_the_shell() {
        let s = Suggestion {
            name: "notes\nid\x1b[A".into(),
            suggestion_type: SuggestionType::File,
            ..Default::default()
        };
        let tail = accept(&s, "cat no").tail;
        assert!(!tail.contains('\n'), "newline reached the line editor");
        assert!(!tail.contains('\x1b'), "escape reached the line editor");
        assert!(tail.contains("notesid[A"));
    }

    /// Per-shell quoting must keep a hostile name inside the quotes: fish
    /// treats `\\` and `\'` as escapes inside single quotes, and PowerShell
    /// treats typographic single quotes as quote characters.
    #[test]
    fn quoting_survives_fish_backslashes_and_powershell_smart_quotes() {
        assert_eq!(
            quote_for_shell("a\\'; id #", Some(Shell::Fish)),
            "'a\\\\\\'; id #'"
        );
        assert_eq!(quote_for_shell("o'b c", Some(Shell::Fish)), "'o\\'b c'");
        assert_eq!(
            quote_for_shell("x\u{2019}; id; \u{2019}y", Some(Shell::Pwsh)),
            "'x\u{2019}\u{2019}; id; \u{2019}\u{2019}y'"
        );
        assert_eq!(quote_for_shell("it's", Some(Shell::Powershell)), "'it''s'");
        assert_eq!(
            quote_for_shell("a\\'; id #", Some(Shell::Xonsh)),
            "'a\\\\\\'; id #'"
        );
        // bash / zsh keep the POSIX splice.
        assert_eq!(quote_for_shell("o'b", Some(Shell::Bash)), "'o'\\''b'");
    }

    /// A filename with a space is one argument, not two.
    #[test]
    fn filenames_needing_quoting_are_quoted() {
        let s = Suggestion {
            name: "my file.txt".into(),
            suggestion_type: SuggestionType::File,
            ..Default::default()
        };
        let repl = accept(&s, "cat my");
        assert_eq!(repl.tail, "\x08\x08'my file.txt'");

        // An apostrophe in the name is spliced out of the quoted string.
        let s = Suggestion {
            name: "o'brien.txt".into(),
            suggestion_type: SuggestionType::File,
            ..Default::default()
        };
        assert_eq!(accept(&s, "cat o").tail, "\x08'o'\\''brien.txt'");
    }

    /// Quoting follows the shell being wrapped. POSIX single-quote rules
    /// produce text cmd.exe cannot run at all (`'…'` are literal characters)
    /// and that PowerShell mangles (backslash is not an escape there), so the
    /// same suggestion must be rendered differently per shell.
    #[test]
    fn quoting_matches_the_wrapped_shell() {
        let spaced = |shell| {
            accept_as(
                &Suggestion {
                    name: "Program Files".into(),
                    suggestion_type: SuggestionType::Folder,
                    ..Default::default()
                },
                "cd Pro",
                Some(shell),
            )
            .tail
        };
        let apostrophe = |shell| {
            accept_as(
                &Suggestion {
                    name: "o'brien.txt".into(),
                    suggestion_type: SuggestionType::File,
                    ..Default::default()
                },
                "cat o",
                Some(shell),
            )
            .tail
        };

        // POSIX shells keep the splice; fish accepts it too because a quote
        // outside single quotes escapes there.
        assert_eq!(spaced(Shell::Bash), "\x08\x08\x08'Program Files'");
        assert_eq!(apostrophe(Shell::Zsh), "\x08'o'\\''brien.txt'");
        // PowerShell: single quotes with doubled quotes.
        assert_eq!(spaced(Shell::Pwsh), "\x08\x08\x08'Program Files'");
        assert_eq!(apostrophe(Shell::Powershell), "\x08'o''brien.txt'");
        // xonsh strings follow Python rules.
        assert_eq!(apostrophe(Shell::Xonsh), "\x08'o\\'brien.txt'");
        // Nushell quotes normally, but has no escape inside single quotes.
        assert_eq!(spaced(Shell::Nu), "\x08\x08\x08'Program Files'");
        assert_eq!(apostrophe(Shell::Nu), "\x08\"o'brien.txt\"");
    }

    #[cfg(windows)]
    #[test]
    fn cmd_receives_double_quotes() {
        let spaced = accept_as(
            &Suggestion {
                name: "Program Files".into(),
                suggestion_type: SuggestionType::Folder,
                ..Default::default()
            },
            "cd Pro",
            Some(Shell::Cmd),
        )
        .tail;
        assert_eq!(spaced, "\x08\x08\x08\"Program Files\"");
    }

    /// Plain names stay unquoted, and `insert_value` is spec-authored shell
    /// text that must be inserted verbatim.
    #[test]
    fn ordinary_names_and_insert_values_are_not_quoted() {
        let s = Suggestion {
            name: "README.md".into(),
            suggestion_type: SuggestionType::File,
            ..Default::default()
        };
        assert_eq!(accept(&s, "cat RE").tail, "ADME.md");

        let s = Suggestion {
            name: "config".into(),
            insert_value: Some("user.name 'value'".into()),
            ..Default::default()
        };
        assert!(accept(&s, "git con").tail.ends_with("user.name 'value'"));
    }

    // ── Upstream replacement.test.ts parity ─────────────────

    #[test]
    fn replacement_no_token_inserts_full() {
        assert_eq!(accept(&sug("status"), "git ").tail, "status");
    }

    #[test]
    fn replacement_divergent_backspaces_all() {
        assert_eq!(accept(&sug("status"), "git xyz").tail, "\x08\x08\x08status");
    }

    #[test]
    fn replacement_insert_value_backspaces_then_inserts() {
        let s = Suggestion {
            name: "status".into(),
            insert_value: Some("status --short".into()),
            ..Default::default()
        };
        // insertValue "status --short" starts with "sta", so the typed text
        // is a prefix and only the tail is written.
        assert_eq!(accept(&s, "git sta").tail, "tus --short");
    }

    #[test]
    fn replacement_option_prefix() {
        assert_eq!(accept(&sug("--version"), "git --ver").tail, "sion");
    }

    #[test]
    fn replacement_empty_name_returns_empty() {
        assert_eq!(accept(&sug(""), "git ").tail, "");
    }

    #[test]
    fn replacement_full_match_returns_empty() {
        assert_eq!(accept(&sug("status"), "git status").tail, "");
    }

    /// A `read` can deliver several keys at once (Windows drains many
    /// KEY_EVENTs into one buffer). Each must be matched on its own, or a
    /// queued `Down Down` matches no binding and closes the popup.
    #[test]
    fn split_keys_separates_coalesced_sequences() {
        assert_eq!(
            split_keys(b"\x1b[B\x1b[B"),
            vec![&b"\x1b[B"[..], &b"\x1b[B"[..]]
        );
        assert_eq!(
            split_keys(b"\x1b[A\t\r"),
            vec![&b"\x1b[A"[..], &b"\t"[..], &b"\r"[..]]
        );
        // SS3 arrows, backtab, and multi-parameter CSI.
        assert_eq!(
            split_keys(b"\x1bOA\x1b[Z"),
            vec![&b"\x1bOA"[..], &b"\x1b[Z"[..]]
        );
        assert_eq!(split_keys(b"\x1b[1;5A"), vec![&b"\x1b[1;5A"[..]]);
        // Lone escape and Alt-b.
        assert_eq!(split_keys(b"\x1b"), vec![&b"\x1b"[..]]);
        assert_eq!(split_keys(b"\x1bb"), vec![&b"\x1bb"[..]]);
    }

    /// Multi-byte characters stay whole so a pasted `é` isn't torn apart.
    #[test]
    fn split_keys_keeps_utf8_scalars_intact() {
        assert_eq!(split_keys("é".as_bytes()), vec!["é".as_bytes()]);
        assert_eq!(
            split_keys("aé日".as_bytes()),
            vec!["a".as_bytes(), "é".as_bytes(), "日".as_bytes()]
        );
    }

    /// #115: bracketed-paste markers survive as single keys so the
    /// event loop can switch to verbatim forwarding.
    #[test]
    fn split_keys_keeps_paste_markers_whole() {
        assert_eq!(
            split_keys(b"\x1b[200~a\tb\x1b[201~"),
            vec![
                &b"\x1b[200~"[..],
                &b"a"[..],
                &b"\t"[..],
                &b"b"[..],
                &b"\x1b[201~"[..]
            ]
        );
    }

    /// A truncated CSI burst must not loop forever or panic.
    #[test]
    fn split_keys_tolerates_truncated_sequences() {
        assert_eq!(split_keys(b"\x1b[1;"), vec![&b"\x1b[1;"[..]]);
        assert_eq!(split_keys(b""), Vec::<&[u8]>::new());
    }

    #[test]
    fn cursor_navigation_clears_visible_ghost_before_forwarding() {
        let pty = FakePty::default();
        let bindings = crate::config::Bindings::default();
        let tracker = TermTracker::new(24, 80);
        let mut renderer =
            Renderer::new(UiMode::Ghost, 5, crate::render::popup::IconSet::default());
        let mut out = Vec::new();
        renderer.draw(&mut out, Some("eckout"), &[], 80).unwrap();
        out.clear();

        let mut pending_tail = history_ghost("eckout");
        let mut popup_mode = PopupMode::Hidden;
        let mut submitting = false;
        let forwarded_cursor_navigation = handle_stdin(
            b"\x1b[D",
            true,
            false,
            &mut pending_tail,
            &mut popup_mode,
            &[],
            "",
            &bindings,
            Shell::Bash,
            &tracker,
            &mut renderer,
            &mut out,
            &pty,
            &mut submitting,
            &current_partial(""),
        );

        assert_eq!(
            forwarded_cursor_navigation,
            InputEffect::ForwardedNavigation
        );
        assert_eq!(pty.writes.borrow().as_slice(), b"\x1b[D");
        assert!(!renderer.ghost_visible());
        assert!(String::from_utf8_lossy(&out).contains(crate::ansi::ERASE_LINE_RIGHT));
    }

    /// Drive one key through `handle_stdin` the way the event loop does and
    /// return what reached the PTY.
    fn press(
        key: &[u8],
        has_popup: bool,
        pending_tail: &mut Option<GhostTail>,
        popup_mode: &mut PopupMode,
        ranked: &[Suggestion],
    ) -> Vec<u8> {
        let pty = FakePty::default();
        let bindings = crate::config::Bindings::default();
        let tracker = TermTracker::new(24, 80);
        let ui = if has_popup {
            UiMode::Hybrid
        } else {
            UiMode::Ghost
        };
        let mut renderer = Renderer::new(ui, 5, crate::render::popup::IconSet::default());
        let mut out = Vec::new();
        let mut submitting = false;
        handle_stdin(
            key,
            true,
            has_popup,
            pending_tail,
            popup_mode,
            ranked,
            // An empty tracker reports an empty line; the suggestions were
            // computed for it.
            "",
            &bindings,
            Shell::Bash,
            &tracker,
            &mut renderer,
            &mut out,
            &pty,
            &mut submitting,
            &current_partial(
                // An empty tracker reports an empty line; the suggestions were
                // computed for it.
                "",
            ),
        );
        pty.writes.into_inner()
    }

    /// #21's own reproduction through the ghost: `cat a` + Right with
    /// `a b.txt` present must insert one quoted word, not two (#86).
    #[test]
    fn ghost_accept_quotes_filenames() {
        let s = Suggestion {
            name: "a b.txt".into(),
            suggestion_type: SuggestionType::File,
            ..Default::default()
        };
        let line = "cat a";
        let ghost = suggestion_ghost(&s, line, &current_partial(line), Some(Shell::Bash)).unwrap();
        // The ghost still reads naturally after the cursor…
        assert_eq!(ghost.display, " b.txt");
        // …but accepting it rewrites the token quoted, like popup accept.
        let mut pending = Some(ghost);
        let wrote = press(b"\x1b[C", false, &mut pending, &mut PopupMode::Hidden, &[]);
        assert_eq!(wrote, b"\x08'a b.txt'");
    }

    /// A `{cursor}` suggestion accepted through the ghost inserts its
    /// `insert_value` and places the caret, as popup accept does. It used to
    /// insert the rest of the bare name.
    #[test]
    fn ghost_accept_honors_insert_value_and_cursor_marker() {
        let s = Suggestion {
            name: "-app".into(),
            insert_value: Some("-app '{cursor}'".into()),
            ..Default::default()
        };
        let line = "cmd -a";
        let ghost = suggestion_ghost(&s, line, &current_partial(line), None).unwrap();
        assert_eq!(ghost.display, "pp");
        assert_eq!(ghost.insert, b"pp ''\x1b[D");
    }

    /// No control character from a suggestion reaches the line editor on
    /// either accept path. A CR would submit the line without Enter (#86).
    #[test]
    fn control_characters_never_reach_the_pty_on_either_accept_path() {
        let s = Suggestion {
            name: "a b\rid\x1b[2J".into(),
            suggestion_type: SuggestionType::File,
            ..Default::default()
        };
        // Ghost accept.
        let line = "cat a";
        let ghost = suggestion_ghost(&s, line, &current_partial(line), Some(Shell::Bash)).unwrap();
        assert!(!ghost.display.chars().any(char::is_control));
        let mut pending = Some(ghost);
        let wrote = press(b"\x1b[C", false, &mut pending, &mut PopupMode::Hidden, &[]);
        assert_eq!(wrote, b"\x08'a bid[2J'");

        // Popup accept.
        let mut mode = PopupMode::Visible { cursor: 0 };
        let wrote = press(b"\t", true, &mut None, &mut mode, std::slice::from_ref(&s));
        assert_eq!(wrote, b"'a bid[2J' ");

        // History ghost.
        let mut pending = history_ghost("tatus\r; id");
        let wrote = press(b"\x1b[C", false, &mut pending, &mut PopupMode::Hidden, &[]);
        assert_eq!(wrote, b"tatus; id");
    }

    /// Generator values (git refs, `package.json` scripts) are quoted on
    /// insert on both accept paths; a ref may legally contain `;` (#86).
    #[test]
    fn generator_values_are_quoted_on_insert() {
        let branch = Suggestion {
            name: "feat;id".into(),
            external: true,
            ..Default::default()
        };
        assert_eq!(accept(&branch, "git checkout fe").tail, "\x08\x08'feat;id'");
        assert_eq!(
            accept_as(&branch, "git checkout fe", Some(Shell::Pwsh)).tail,
            "\x08\x08'feat;id'"
        );

        let line = "git checkout fe";
        let ghost =
            suggestion_ghost(&branch, line, &current_partial(line), Some(Shell::Bash)).unwrap();
        assert_eq!(ghost.display, "at;id");
        assert_eq!(ghost.insert, b"\x08\x08'feat;id'");

        let mut mode = PopupMode::Visible { cursor: 0 };
        let wrote = press(
            b"\t",
            true,
            &mut None,
            &mut mode,
            std::slice::from_ref(&branch),
        );
        assert_eq!(wrote, b"'feat;id' ");

        // A plain generator value needs no quotes.
        let plain = Suggestion {
            name: "feature/login".into(),
            external: true,
            ..Default::default()
        };
        assert_eq!(accept(&plain, "git checkout fe").tail, "ature/login");
    }

    /// A spec's own suggestion names are shell text, like `insert_value`:
    /// `find -exec … \;` must insert `\;`, not the literal `'\;'`.
    #[test]
    fn spec_authored_names_are_not_quoted() {
        assert_eq!(accept(&sug("\\;"), "find . -exec ls {} ").tail, "\\;");
        assert_eq!(accept(&sug("~"), "cd ").tail, "~");
    }

    #[test]
    fn right_arrow_without_pending_ghost_is_navigation() {
        let pty = FakePty::default();
        let bindings = crate::config::Bindings::default();
        let tracker = TermTracker::new(24, 80);
        let mut renderer =
            Renderer::new(UiMode::Ghost, 5, crate::render::popup::IconSet::default());
        let mut out = Vec::new();
        let mut pending_tail = None;
        let mut popup_mode = PopupMode::Hidden;
        let mut submitting = false;

        let forwarded_cursor_navigation = handle_stdin(
            b"\x1b[C",
            true,
            false,
            &mut pending_tail,
            &mut popup_mode,
            &[],
            "",
            &bindings,
            Shell::Bash,
            &tracker,
            &mut renderer,
            &mut out,
            &pty,
            &mut submitting,
            &current_partial(""),
        );

        assert_eq!(
            forwarded_cursor_navigation,
            InputEffect::ForwardedNavigation
        );
        assert_eq!(pty.writes.borrow().as_slice(), b"\x1b[C");
    }
}
