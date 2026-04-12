//! PTY wrapper — spawn the user's shell under portable-pty, proxy I/O
//! to/from the user's real terminal, filter OSC 6973 out, feed a headless
//! vt100 parser, and drive either the ghost-text or interactive popup
//! renderer after each refresh.

use crate::ansi;
use crate::config::{Bindings, UiMode};
use crate::history;
use crate::paths;
use crate::render::{Direction, Renderer};
use crate::shell::Shell;
use crate::spec::model::{Suggestion, SuggestionType};
use crate::spec::Registry;
use crate::suggest::Engine;
use crate::term::TermTracker;
use anyhow::{Context, Result};
use crossterm::terminal::{self, ClearType};
use portable_pty::{CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

    let shell_path = find_on_path(shell.as_str())
        .with_context(|| format!("shell not found on PATH: {}", shell.as_str()))?;
    let shell_dir = paths::shell_dir().context("no HOME directory")?;
    let zsh_dotdir = paths::zsh_dotdir().context("no HOME directory")?;
    let target = shell.spawn_target(&shell_dir, &zsh_dotdir, login);

    let (cols, rows) = terminal::size()
        .ok()
        .filter(|(c, r)| *c > 0 && *r > 0)
        .unwrap_or((80, 24));
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty failed")?;

    let mut cmd = CommandBuilder::new(&shell_path);
    for arg in &target.args {
        cmd.arg(arg);
    }
    // Dual-guard: both ISTERM and INSH_RS are set so this coexists with
    // upstream inshellisense's shell integration.
    cmd.env("ISTERM", "1");
    cmd.env("INSH_RS", "1");
    if login {
        cmd.env("ISTERM_LOGIN", "1");
        cmd.env("INSH_RS_LOGIN", "1");
    }
    cmd.env("TERM", "xterm-256color");
    for (k, v) in &target.env {
        cmd.env(k, v);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    let mut child = pair.slave.spawn_command(cmd).context("failed to spawn bash")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone reader")?;
    let mut writer = pair.master.take_writer().context("take writer")?;

    terminal::enable_raw_mode().ok();

    // Clear the host terminal on startup — matches upstream's
    // `writeOutput(ansi.clearTerminal)` in ui-root.ts. Without this,
    // any leftover output from before `insh start` was invoked stays
    // visible and interferes with the popup's cursor-relative draws
    // (the first popup ends up rendering inside whatever stale text
    // happened to be on screen). `\x1b[2J` erases the visible screen,
    // `\x1b[3J` erases the scrollback, `\x1b[H` moves the cursor to
    // the home position.
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[2J\x1b[3J\x1b[H").ok();
        out.flush().ok();
    }

    // Load the 1470-spec registry on a background thread so the
    // user's shell prompt appears immediately (~5ms) instead of
    // blocking ~500ms on zstd decompression + JSON parsing. Popup
    // suggestions are gated on the engine being ready — until then,
    // typed keystrokes still reach bash normally and the popup just
    // stays hidden. The engine typically finishes loading before
    // the user has finished typing their first partial command.
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

    let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>();
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();

    // Reader thread: PTY -> channel
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Stdin thread: user keys -> channel
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 1024];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

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
    let mut renderer = Renderer::new(std::io::stdout(), effective_ui, cfg.max_suggestions);

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

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let _ = status;
            break;
        }

        // Poll for SIGWINCH — crossterm::terminal::size() is a single
        // TIOCGWINSZ ioctl, cheap enough per tick. If dimensions
        // changed, tell the vt parser and the slave PTY so readline
        // re-wraps at the new width. Guard against (0, 0) which some
        // PTY allocators report transiently — feeding bytes into a
        // zero-sized vt100 grid panics.
        let mut made_progress = false;
        if let Ok((new_cols, new_rows)) = terminal::size() {
            if new_cols > 0
                && new_rows > 0
                && (new_rows != tracker.rows() || new_cols != tracker.cols())
            {
                tracker.resize(new_rows, new_cols);
                let _ = pair.master.resize(PtySize {
                    rows: new_rows,
                    cols: new_cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                renderer.clear().ok();
                made_progress = true;
            }
        }

        // Drain PTY output.
        while let Ok(bytes) = pty_rx.try_recv() {
            let (clean, events) = ansi::scan(&bytes);
            out.write_all(&clean).ok();
            out.flush().ok();
            tracker.feed(&clean, &events);
            made_progress = true;
        }

        // Drain user stdin and forward to PTY, intercepting popup keys
        // when the popup is visible and ghost-accept keys when a ghost
        // tail is pending.
        while let Ok(bytes) = stdin_rx.try_recv() {
            // Ghost-accept (right / End / Ctrl-E) works in Ghost and
            // Hybrid modes. It accepts whatever tail the renderer is
            // currently showing — which in Hybrid mode is the tail of
            // the popup's *active* entry, not just the top.
            if has_ghost {
                let ghost_accept: &[&[u8]] = &[b"\x1b[C", b"\x1b[F", b"\x05"];
                if ghost_accept.contains(&bytes.as_slice()) {
                    if let Some(tail) = pending_tail.take() {
                        renderer.clear().ok();
                        writer.write_all(tail.as_bytes()).ok();
                        writer.flush().ok();
                        popup_mode = PopupMode::Hidden;
                        made_progress = true;
                        continue;
                    }
                }
            }

            if has_popup {
                if let PopupMode::Visible { cursor } = popup_mode {
                    if !ranked.is_empty() {
                        if bindings.next_suggestion.matches(&bytes) {
                            let new_cursor = (cursor + 1).min(ranked.len() - 1);
                            popup_mode = PopupMode::Visible { cursor: new_cursor };
                            made_progress = true;
                            continue;
                        }
                        if bindings.previous_suggestion.matches(&bytes) {
                            let new_cursor = cursor.saturating_sub(1);
                            popup_mode = PopupMode::Visible { cursor: new_cursor };
                            made_progress = true;
                            continue;
                        }
                        if bindings.accept_suggestion.matches(&bytes) {
                            let selected = &ranked[cursor.min(ranked.len() - 1)];
                            let partial = current_partial(&tracker.state().command);
                            let tail = replacement_tail(selected, &partial);
                            renderer.clear().ok();
                            if !tail.is_empty() {
                                writer.write_all(tail.as_bytes()).ok();
                            }
                            if !matches!(selected.suggestion_type, SuggestionType::Folder) {
                                writer.write_all(b" ").ok();
                            }
                            writer.flush().ok();
                            popup_mode = PopupMode::Hidden;
                            pending_tail = None;
                            made_progress = true;
                            continue;
                        }
                        if bindings.dismiss_suggestions.matches(&bytes) {
                            renderer.clear().ok();
                            popup_mode = PopupMode::Dismissed;
                            made_progress = true;
                            continue;
                        }
                        // Anything else: close the popup and fall
                        // through to forward the byte into the shell.
                        renderer.clear().ok();
                        popup_mode = PopupMode::Hidden;
                    }
                }
            }

            // Default: clear any drawn UI (bash will repaint the same
            // cells) and forward the bytes into the shell.
            //
            // EXCEPT: if the key is pure cursor navigation (left/right
            // arrow, up/down history, home/end, Ctrl-A/E while the
            // cursor is in the middle of typed text), do NOT clear.
            // GhostRenderer::clear() emits `\x1b[K` (erase-line-right)
            // from the current cursor position — which wipes the
            // user's typed characters AHEAD of the cursor when they
            // arrow-left into the middle of a command. Navigation
            // keys don't modify the command text, so the ghost is
            // still valid and should be left alone.
            if !is_cursor_navigation(&bytes) {
                renderer.clear().ok();
            }
            // If the user is submitting a command (Enter or Ctrl-C),
            // latch the `submitting` flag so the next redraw
            // suppresses the popup until a fresh PromptStart arrives.
            // `bytes` may be a 1- or 2-byte chunk: `\r`, `\n`, `\r\n`,
            // or `\x03` (SIGINT).
            if bytes.iter().any(|&b| b == b'\r' || b == b'\n' || b == 0x03) {
                submitting = true;
                popup_mode = PopupMode::Hidden;
            }
            writer.write_all(&bytes).ok();
            writer.flush().ok();
            made_progress = true;
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
                    renderer.clear().ok();
                    last_cmd_signature.clear();
                } else {
                    // Still post-Enter but pre-PromptStart. Make sure
                    // nothing is drawn this tick.
                    renderer.clear().ok();
                }
            } else if state.command.is_empty() {
                ranked.clear();
                pending_tail = None;
                popup_mode = PopupMode::Hidden;
                renderer.clear().ok();
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
                let engine_guard = engine.read().ok();
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
                    renderer.clear().ok();
                } else if !has_popup {
                    // Pure Ghost mode.
                    renderer.draw(tail.as_deref(), &ranked).ok();
                }

                last_cmd_signature = state.command.clone();
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }

    terminal::disable_raw_mode().ok();
    let _ = crossterm::execute!(std::io::stdout(), terminal::Clear(ClearType::CurrentLine));
    Ok(())
}

fn find_on_path(binary: &str) -> Result<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let candidate = std::path::Path::new(dir).join(binary);
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("not found on PATH: {}", binary)
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
        // Ctrl-A (home) — Ctrl-E is intentionally EXCLUDED here
        // because it's a ghost-accept key in our bindings, and is
        // handled in the accept-path before this.
        | b"\x01"
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
