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
use crate::spec::model::{Suggestion, SuggestionType};
use crate::spec::Registry;
use crate::suggest::Engine;
use crate::term::TermTracker;
use anyhow::{Context, Result};
use std::io::Write;
use std::thread;

pub fn run_wrapped_shell() -> Result<()> {
    run_wrapped(Shell::Bash, false, None)
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
/// config. This is how `insh start --ui popup` reaches the renderer.
pub fn run_wrapped(shell: Shell, login: bool, ui_override: Option<UiMode>) -> Result<()> {
    // Make sure the vendored shell integration scripts are on disk.
    let _ = crate::resources::unpack();

    let shell_path = find_shell_binary(shell)?;
    let shell_dir = paths::shell_dir().context("no HOME directory")?;
    let zsh_dotdir = paths::zsh_dotdir().context("no HOME directory")?;
    let target = shell.spawn_target(&shell_dir, &zsh_dotdir, login);

    let (cols, rows) = platform::term_size().unwrap_or((80, 24));

    // Build the environment for the child shell.
    let mut child_env: Vec<(String, String)> = vec![
        ("ISTERM".into(), "1".into()),
        ("INSH_RS".into(), "1".into()),
        ("TERM".into(), "xterm-256color".into()),
    ];
    if login {
        child_env.push(("ISTERM_LOGIN".into(), "1".into()));
        child_env.push(("INSH_RS_LOGIN".into(), "1".into()));
    }
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

    platform::enable_raw_mode();
    platform::install_signal_handlers();

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        platform::disable_raw_mode();
        prev_hook(info);
    }));

    // Clear the host terminal on startup — matches upstream's
    // `writeOutput(ansi.clearTerminal)` in ui-root.ts. Without this,
    // any leftover output from before `insh start` was invoked stays
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
    {
        let engine = engine.clone();
        thread::spawn(move || {
            let registry = Registry::new_with_defaults();
            let hist = history::load();
            let built = Engine::new(registry, hist);
            if let Ok(mut slot) = engine.write() {
                *slot = Some(built);
            }
        });
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut tracker = TermTracker::new(rows, cols);
    // Popup / ghost dispatch: CLI --ui flag (ui_override) beats config
    // file, which beats the built-in default (Ghost).
    let cfg = crate::config::load();
    let effective_ui = ui_override.unwrap_or(cfg.ui);
    let has_popup = matches!(effective_ui, UiMode::Popup | UiMode::Hybrid);
    let has_ghost = matches!(effective_ui, UiMode::Ghost | UiMode::Hybrid);
    let bindings: Bindings = cfg.bindings.clone();
    let max_popup_rows = cfg.max_suggestions.max(1) as usize + 1 /* desc line */ + 4 /* desc overflow headroom */;
    let mut renderer = Renderer::new(effective_ui, cfg.max_suggestions);

    // Shared suggestion state.
    let mut pending_tail: Option<String> = None; // ghost mode
    let mut ranked: Vec<Suggestion> = Vec::new();
    let mut popup_mode = PopupMode::Hidden;
    let mut last_cmd_signature = String::new();
    // Set to true after the user submits a command (Enter/Ctrl-C) so the
    // next redraw is suppressed until bash emits the next PromptStart.
    // Without this we race bash's echo: we clear the popup, forward `\r`,
    // then the same tick's redraw re-emits the popup because
    // `state.command` still points at the old text — and by the time
    // bash scrolls the screen, the new popup top-border is stranded
    // wherever the cursor happened to be.
    let mut submitting = false;

    let mut pty_buf = [0u8; 4096];
    let mut stdin_buf = [0u8; 1024];

    loop {
        if pty.try_wait().is_some() {
            // Drain remaining PTY output before shutting down.
            // The child's final output (including console-mode-reset
            // sequences) must reach the parent terminal.
            loop {
                let n = pty.read_pty(&mut pty_buf);
                if n < 1 {
                    break;
                }
                let bytes = &pty_buf[..n as usize];
                let (clean, _) = ansi::scan(bytes);
                out.write_all(&clean).ok();
            }
            out.flush().ok();
            break;
        }

        // SIGWINCH — one ioctl per wakeup.
        if let Some((new_cols, new_rows)) = platform::term_size() {
            if new_cols > 0
                && new_rows > 0
                && (new_rows != tracker.rows() || new_cols != tracker.cols())
            {
                tracker.resize(new_rows, new_cols);
                pty.resize(new_rows, new_cols);
                renderer.clear(&mut out).ok();
            }
        }

        // Block until data arrives on PTY or stdin (0% CPU idle).
        let (pty_ready, stdin_ready) = pty.poll(50);
        if !pty_ready && !stdin_ready {
            continue;
        }

        let mut made_progress = false;

        // Read PTY output if ready.
        if pty_ready {
            loop {
                let n = pty.read_pty(&mut pty_buf);
                if n < 1 {
                    break;
                }
                let bytes = &pty_buf[..n as usize];
                let (clean, osc_events) = ansi::scan(bytes);
                out.write_all(&clean).ok();
                out.flush().ok();
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
            // Fallback for ConPTY: OSC 6973 markers are stripped, so
            // the prompt anchor is never set via events. Infer it from
            // the cursor position the moment the user starts typing —
            // the cursor is sitting right after the prompt.
            if !tracker.has_prompt_anchor() {
                tracker.set_fallback_anchor();
            }
            let n = pty.read_stdin(&mut stdin_buf);
            if n > 0 {
                let bytes = stdin_buf[..n as usize].to_vec();
                handle_stdin(
                    &bytes,
                    has_ghost,
                    has_popup,
                    &mut pending_tail,
                    &mut popup_mode,
                    &ranked,
                    &bindings,
                    &tracker,
                    &mut renderer,
                    &mut out,
                    &pty,
                    &mut submitting,
                );
                made_progress = true;
            }
        }

        if made_progress {
            let state = tracker.state().clone();
            let cwd = tracker.cwd().to_string();

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
                // Re-arm a dismissed popup on any edit to the line.
                if popup_mode == PopupMode::Dismissed
                    && state.command != last_cmd_signature
                {
                    popup_mode = PopupMode::Hidden;
                }

                // The engine loads on a background thread; if it
                // hasn't finished yet, this tick is a noop for
                // suggestions. Typed keystrokes still reach bash
                // normally — the user just doesn't see the popup for
                // the ~500ms it takes the registry to load.
                let engine_guard = engine.read().map_err(|e| {
                    eprintln!("is: engine lock poisoned: {e}");
                }).ok();
                let Some(engine_ref) = engine_guard.as_ref().and_then(|g| g.as_ref()) else {
                    last_cmd_signature = state.command.clone();
                    continue;
                };

                ranked = engine_ref.suggest_blob(&state.command, &cwd);

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
                    let partial = current_partial(&state.command);
                    let t = replacement_tail(&ranked[active_cursor], &partial);
                    // The replacement_tail helper returns a string that
                    // may start with backspaces if the suggestion has a
                    // different prefix from the partial — those aren't
                    // safe to display as ghost text. Fall back to the
                    // engine's tail for display, while still using the
                    // richer `replacement_tail` value at accept time.
                    if t.starts_with('\x08') {
                        engine_ref.suggest(&state.command, &cwd)
                    } else {
                        Some(t)
                    }
                } else if has_ghost && cursor_at_end {
                    engine_ref.suggest(&state.command, &cwd)
                } else {
                    None
                };
                pending_tail = tail.clone();

                if has_popup && popup_mode != PopupMode::Dismissed && !ranked.is_empty() {
                    let popup_direction =
                        if (tracker.remaining_lines() as usize) > max_popup_rows {
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
                    renderer.draw(&mut out, tail.as_deref(), &ranked).ok();
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
    // On Windows, process::exit lets the OS tear down ConPTY.
    // Calling ClosePseudoConsole explicitly causes deadlocks
    // and white flashes in Windows Terminal.
    #[cfg(windows)]
    std::process::exit(0);
    #[allow(unreachable_code)]
    {
        let _ = std::panic::take_hook();
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_stdin(
    bytes: &[u8],
    has_ghost: bool,
    has_popup: bool,
    pending_tail: &mut Option<String>,
    popup_mode: &mut PopupMode,
    ranked: &[Suggestion],
    bindings: &Bindings,
    tracker: &TermTracker,
    renderer: &mut Renderer,
    out: &mut impl Write,
    pty: &dyn PtyHandle,
    submitting: &mut bool,
) {
    // Ghost-accept (right / End / Ctrl-E).
    if has_ghost {
        let ghost_accept: &[&[u8]] = &[b"\x1b[C", b"\x1b[F", b"\x05"];
        if ghost_accept.contains(&bytes) {
            if let Some(tail) = pending_tail.take() {
                renderer.clear(out).ok();
                pty.pty_write(tail.as_bytes());
                *popup_mode = PopupMode::Hidden;
                return;
            }
        }
    }
    // Popup key interception.
    if has_popup {
        if let PopupMode::Visible { cursor } = *popup_mode {
            if !ranked.is_empty() {
                if bindings.next_suggestion.matches(bytes) {
                    *popup_mode = PopupMode::Visible {
                        cursor: (cursor + 1).min(ranked.len() - 1),
                    };
                    return;
                }
                if bindings.previous_suggestion.matches(bytes) {
                    *popup_mode = PopupMode::Visible {
                        cursor: cursor.saturating_sub(1),
                    };
                    return;
                }
                if bindings.accept_suggestion.matches(bytes) {
                    let selected = &ranked[cursor.min(ranked.len() - 1)];
                    let partial = current_partial(&tracker.state().command);
                    let tail = replacement_tail(selected, &partial);
                    renderer.clear(out).ok();
                    if !tail.is_empty() {
                        pty.pty_write(tail.as_bytes());
                    }
                    if !matches!(selected.suggestion_type, SuggestionType::Folder) {
                        pty.pty_write(b" ");
                    }
                    *popup_mode = PopupMode::Hidden;
                    *pending_tail = None;
                    return;
                }
                if bindings.dismiss_suggestions.matches(bytes) {
                    renderer.clear(out).ok();
                    *popup_mode = PopupMode::Dismissed;
                    return;
                }
                // Anything else: close popup, fall through.
                renderer.clear(out).ok();
                *popup_mode = PopupMode::Hidden;
            }
        }
    }
    // Default forward path.
    if !is_cursor_navigation(bytes) {
        renderer.clear(out).ok();
    }
    if bytes.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0x03) {
        *submitting = true;
        *popup_mode = PopupMode::Hidden;
    }
    pty.pty_write(bytes);
}

/// Is this stdin chunk a pure cursor-navigation key that doesn't
/// modify the command text? These keys should NOT trigger a clear of
/// the ghost renderer because clearing emits `\x1b[K` (erase line
/// right) which wipes any typed characters ahead of the cursor.
fn is_cursor_navigation(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        // Arrow keys (CSI form and SS3 form)
        b"\x1b[A" | b"\x1b[B" | b"\x1b[D"
        | b"\x1bOA" | b"\x1bOB" | b"\x1bOD"
        // Home / End (CSI, VT, SS3 variants)
        | b"\x1b[H" | b"\x1b[1~" | b"\x1bOH"
        // Page up / down
        | b"\x1b[5~" | b"\x1b[6~"
        // Ctrl-A (home), Ctrl-B (back-char), Ctrl-F (forward-char).
        // Ctrl-E is intentionally EXCLUDED — it's a ghost-accept
        // key handled in the accept path before this.
        | b"\x01" | b"\x02" | b"\x06"
        // Meta-B / Meta-F (word navigation in bash)
        | b"\x1bb" | b"\x1bf"
    )
}

/// Current partial token being typed on the command line — the substring
/// after the last run of whitespace. Empty when the line ends on a space.
fn current_partial(command: &str) -> String {
    match command.rsplit_once(char::is_whitespace) {
        Some((_, tail)) => tail.to_string(),
        None => command.to_string(),
    }
}

/// Given an accepted suggestion and the current partial token, return the
/// bytes we need to write into the shell so the line matches
/// `suggestion.name` (or `insert_value` when set).
fn replacement_tail(suggestion: &Suggestion, partial: &str) -> String {
    let target: &str = suggestion
        .insert_value
        .as_deref()
        .unwrap_or(suggestion.name.as_str());
    if let Some(rest) = target.strip_prefix(partial) {
        rest.to_string()
    } else {
        // Fall back to clearing the partial with backspaces and writing
        // the full target.
        let mut s = String::new();
        for _ in 0..partial.chars().count() {
            s.push('\x08');
        }
        s.push_str(target);
        s
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

    #[test]
    fn partial_trailing_word() {
        assert_eq!(current_partial("git ch"), "ch");
        assert_eq!(current_partial("git"), "git");
        assert_eq!(current_partial("git "), "");
    }

    #[test]
    fn replacement_extends_partial() {
        let s = Suggestion {
            name: "checkout".into(),
            ..Default::default()
        };
        assert_eq!(replacement_tail(&s, "ch"), "eckout");
    }

    #[test]
    fn replacement_backspaces_when_prefix_mismatches() {
        let s = Suggestion {
            name: "pull".into(),
            ..Default::default()
        };
        let out = replacement_tail(&s, "ch");
        assert!(out.starts_with("\x08\x08"));
        assert!(out.ends_with("pull"));
    }

    #[test]
    fn replacement_uses_insert_value_when_set() {
        let s = Suggestion {
            name: "foo".into(),
            insert_value: Some("foo={cursor}".into()),
            ..Default::default()
        };
        let out = replacement_tail(&s, "fo");
        assert_eq!(out, "o={cursor}");
    }
}
