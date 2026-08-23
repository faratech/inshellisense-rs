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

    let cfg = crate::config::load();

    // Background engine load — ~500ms of zstd + JSON parse off the
    // critical path so the shell prompt appears instantly.
    let engine: std::sync::Arc<std::sync::RwLock<Option<Engine>>> =
        std::sync::Arc::new(std::sync::RwLock::new(None));
    {
        let engine = engine.clone();
        let alias_shell = shell;
        let use_aliases = cfg.use_aliases;
        thread::spawn(move || {
            let registry = Registry::new_with_defaults();
            let hist = history::load();
            let mut built = Engine::new(registry, hist);
            built.set_shell(alias_shell);
            if use_aliases {
                built.set_aliases(crate::alias::load(alias_shell));
            }
            if let Ok(mut slot) = engine.write() {
                *slot = Some(built);
            }
        });
    }

    // Suggestion worker. Generators shell out (`git branch`, `docker ps`, …)
    // with a 5-second default timeout, and `suggest_blob` used to run on this
    // thread — so a slow generator froze keystroke handling for as long as it
    // took. Compute suggestions off the event loop and deliver them by channel.
    type SuggestRequest = (u64, String, String);
    type SuggestResponse = (u64, Vec<Suggestion>, Option<String>);
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
                    let _ = res_tx.send((sig, Vec::new(), None));
                    continue;
                };
                let ranked = engine_ref.suggest_blob(&typed, &cwd);
                let tail = engine_ref.suggest(&typed, &cwd);
                if res_tx.send((sig, ranked, tail)).is_err() {
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
    let effective_ui = ui_override.unwrap_or(cfg.ui);
    let has_popup = matches!(effective_ui, UiMode::Popup | UiMode::Hybrid);
    let has_ghost = matches!(effective_ui, UiMode::Ghost | UiMode::Hybrid);
    let bindings: Bindings = cfg.bindings.clone();
    let max_popup_rows = cfg.max_suggestions.max(1) as usize + 1 /* desc line */ + 4 /* desc overflow headroom */;
    let mut renderer = Renderer::new(
        effective_ui,
        cfg.max_suggestions,
        crate::render::popup::IconSet::from_config(cfg.use_nerd_font),
    );

    // Shared suggestion state.
    let mut pending_tail: Option<String> = None; // ghost mode
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
    // Tracks the background registry load so the first tick after it lands
    // can redraw the suggestion for text already on the line.
    let mut engine_was_ready = false;
    // Signature of the text the outstanding request was made for, and the text
    // the suggestions currently in `ranked` were computed from.
    let mut request_sig: u64 = 0;
    let mut ranked_typed = String::new();
    let mut engine_tail: Option<String> = None;

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
            // we keep draining while data flows and stop ~500ms after it
            // goes quiet.
            let mut quiet_ticks: u32 = 0;
            loop {
                let n = pty.read_pty(&mut pty_buf);
                if n >= 1 {
                    quiet_ticks = 0;
                    let bytes = &pty_buf[..n as usize];
                    let (clean, _) = ansi_scanner.scan(bytes);
                    out.write_all(&clean).ok();
                    continue;
                }
                quiet_ticks += 1;
                if quiet_ticks > 25 {
                    break;
                }
                let (pty_ready, _) = pty.poll(20);
                if !pty_ready && quiet_ticks > 2 {
                    break;
                }
            }
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
        let engine_ready = engine.read().map(|g| g.is_some()).unwrap_or(false);
        let engine_just_loaded = engine_ready && !engine_was_ready;
        engine_was_ready = engine_ready;

        // Pick up anything the suggestion worker finished while we were idle.
        let mut got_suggestions = false;
        while let Ok((sig, new_ranked, new_tail)) = res_rx.try_recv() {
            if sig == request_sig {
                ranked = new_ranked;
                engine_tail = new_tail;
                got_suggestions = true;
            }
        }

        if !pty_ready && !stdin_ready && !engine_just_loaded && !got_suggestions {
            continue;
        }

        // Suggestions arriving from the worker are progress too: without this
        // a tick woken only by a worker result would fall through the redraw.
        let mut made_progress = got_suggestions;
        let mut defer_suggestion_redraw = false;

        // Read PTY output if ready.
        if pty_ready {
            loop {
                let n = pty.read_pty(&mut pty_buf);
                if n < 1 {
                    break;
                }
                let bytes = &pty_buf[..n as usize];
                let (clean, osc_events) = ansi_scanner.scan(bytes);
                out.write_all(&clean).ok();
                out.flush().ok();
                // A new prompt means the previous command may have been
                // appended to the shell's history file. Reload it so ghost
                // text can suggest what the user just ran.
                if osc_events
                    .iter()
                    .any(|e| matches!(e, ansi::IsEvent::PromptStart))
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
                tracker.feed(&clean, &osc_events);
                pending_tail = None;
                made_progress = true;
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
                    // Track Up/Down for history-hide behavior (upstream:
                    // suggestionManager.ts:172-174).
                    last_stdin_was_history = bytes == b"\x1b[A"
                        || bytes == b"\x1b[B"
                        || bytes == b"\x1bOA"
                        || bytes == b"\x1bOB";
                    let forwarded_cursor_navigation = handle_stdin(
                        bytes,
                        has_ghost,
                        has_popup,
                        &mut pending_tail,
                        &mut popup_mode,
                        &ranked,
                        &ranked_typed,
                        &bindings,
                        &tracker,
                        &mut renderer,
                        &mut out,
                        &pty,
                        &mut submitting,
                    );
                    if forwarded_cursor_navigation {
                        pending_tail = None;
                        defer_suggestion_redraw = true;
                    }
                }
                made_progress = true;
            }
        }

        if made_progress {
            let state = tracker.state().clone();
            let cwd = tracker.cwd().to_string();
            if defer_suggestion_redraw {
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
                let sig = suggestion_signature(&typed, &cwd);
                if sig != request_sig {
                    request_sig = sig;
                    if req_tx.send((sig, typed.clone(), cwd.clone())).is_err() {
                        break;
                    }
                }
                if got_suggestions {
                    ranked_typed = typed.clone();
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
                let tail = if !ranked.is_empty() && has_ghost && cursor_at_end {
                    let partial = current_partial(&typed);
                    let repl = replacement_tail(&ranked[active_cursor], &typed, &partial);
                    // A replacement that erases the token starts with
                    // backspaces, and one with a `{cursor}` needs a caret
                    // move — neither is displayable as inline ghost text.
                    // Fall back to the engine's plain tail for display; the
                    // richer `Replacement` is rebuilt at accept time.
                    if repl.tail.starts_with('\x08') || repl.had_marker {
                        engine_tail.clone()
                    } else {
                        Some(repl.tail)
                    }
                } else if has_ghost && cursor_at_end {
                    engine_tail.clone()
                } else {
                    None
                };
                pending_tail = tail.clone();

                if has_popup && popup_mode != PopupMode::Dismissed && !ranked.is_empty() {
                    // Flipping to `Above` whenever there was not enough room
                    // below ignored whether there was any room *above*. With
                    // the prompt near the top of a short terminal the popup was
                    // drawn off the top of the screen.
                    let rows_below = tracker.remaining_lines() as usize;
                    let rows_above = tracker.state().cursor_row as usize;
                    let popup_direction = if rows_below > max_popup_rows {
                        Direction::Below
                    } else if rows_above >= max_popup_rows {
                        Direction::Above
                    } else if rows_below >= rows_above {
                        Direction::Below
                    } else {
                        Direction::Above
                    };
                    let cursor_col = tracker.state().cursor_col;
                    let term_cols = tracker.cols();
                    renderer
                        .draw_popup_interactive(
                            &mut out,
                            tail.as_deref(),
                            &ranked,
                            active_cursor,
                            popup_direction,
                            cursor_col,
                            term_cols,
                        )
                        .ok();
                    popup_mode = PopupMode::Visible {
                        cursor: active_cursor,
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

#[allow(clippy::too_many_arguments)]
fn handle_stdin(
    bytes: &[u8],
    has_ghost: bool,
    has_popup: bool,
    pending_tail: &mut Option<String>,
    popup_mode: &mut PopupMode,
    ranked: &[Suggestion],
    // The text `ranked` was computed from. Suggestions are produced off the
    // event loop, so they can lag the line by a tick.
    ranked_typed: &str,
    bindings: &Bindings,
    tracker: &TermTracker,
    renderer: &mut Renderer,
    out: &mut impl Write,
    pty: &dyn PtyHandle,
    submitting: &mut bool,
) -> bool {
    // Ghost-accept (right / End / Ctrl-E).
    if has_ghost {
        let ghost_accept: &[&[u8]] = &[b"\x1b[C", b"\x1b[F", b"\x05"];
        if ghost_accept.contains(&bytes)
            && let Some(tail) = pending_tail.take()
        {
            renderer.clear(out).ok();
            pty.pty_write(tail.as_bytes());
            *popup_mode = PopupMode::Hidden;
            return false;
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
            return false;
        }
        if bindings.previous_suggestion.matches(bytes) {
            *popup_mode = PopupMode::Visible {
                cursor: cursor.saturating_sub(1),
            };
            return false;
        }
        if bindings.accept_suggestion.matches(bytes) {
            let typed = tracker.command_before_cursor();
            // Never accept a suggestion computed for different text —
            // its replacement span would not match the current line.
            if typed != ranked_typed {
                renderer.clear(out).ok();
                *popup_mode = PopupMode::Hidden;
                pty.pty_write(bytes);
                return false;
            }
            let selected = &ranked[cursor.min(ranked.len() - 1)];
            let partial = current_partial(&typed);
            let repl = replacement_tail(selected, &typed, &partial);
            renderer.clear(out).ok();
            if !repl.tail.is_empty() {
                pty.pty_write(repl.tail.as_bytes());
            }
            // A `{cursor}` suggestion places the caret inside the
            // inserted text, so no separating space belongs after it.
            let wants_space =
                !repl.had_marker && !matches!(selected.suggestion_type, SuggestionType::Folder);
            if wants_space {
                pty.pty_write(b" ");
            }
            let caret = repl.caret_move();
            if !caret.is_empty() {
                pty.pty_write(&caret);
            }
            *popup_mode = PopupMode::Hidden;
            *pending_tail = None;
            return false;
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
    if bytes.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0x03) {
        *submitting = true;
        *popup_mode = PopupMode::Hidden;
    }
    pty.pty_write(bytes);
    forwarded_cursor_navigation
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

/// Wrap a filesystem name in POSIX single quotes so spaces and metacharacters
/// insert literally. Applied only to File/Folder suggestions, whose names come
/// from the filesystem; an `insert_value` is shell text authored by the spec
/// and must be inserted verbatim.
fn quote_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Given an accepted suggestion and the line so far, the bytes to write into
/// the shell so the line ends with `insert_value` (or `name`).
fn replacement_tail(suggestion: &Suggestion, line: &str, partial: &Partial) -> Replacement {
    let (target, cursor_left, had_marker) = match suggestion.insert_value.as_deref() {
        Some(raw) => split_cursor_marker(&strip_control_chars(raw)),
        None => {
            let name = strip_control_chars(&suggestion.name);
            let quote = matches!(
                suggestion.suggestion_type,
                SuggestionType::File | SuggestionType::Folder
            ) && needs_quoting(&name);
            let rendered = if quote { quote_word(&name) } else { name };
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
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakePty {
        writes: RefCell<Vec<u8>>,
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
            (false, false)
        }

        fn read_pty(&self, _buf: &mut [u8]) -> isize {
            -1
        }

        fn read_stdin(&self, _buf: &mut [u8]) -> isize {
            -1
        }

        fn close(&mut self) {}
    }

    /// Accept `s` against the whole line, deriving the partial the way the
    /// event loop does.
    fn accept(s: &Suggestion, line: &str) -> Replacement {
        let partial = current_partial(line);
        replacement_tail(s, line, &partial)
    }

    fn sug(name: &str) -> Suggestion {
        Suggestion {
            name: name.into(),
            ..Default::default()
        }
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

        let mut pending_tail = Some("eckout".to_string());
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
            &tracker,
            &mut renderer,
            &mut out,
            &pty,
            &mut submitting,
        );

        assert!(forwarded_cursor_navigation);
        assert_eq!(pty.writes.borrow().as_slice(), b"\x1b[D");
        assert!(!renderer.ghost_visible());
        assert!(String::from_utf8_lossy(&out).contains(crate::ansi::ERASE_LINE_RIGHT));
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
            &tracker,
            &mut renderer,
            &mut out,
            &pty,
            &mut submitting,
        );

        assert!(forwarded_cursor_navigation);
        assert_eq!(pty.writes.borrow().as_slice(), b"\x1b[C");
    }
}
