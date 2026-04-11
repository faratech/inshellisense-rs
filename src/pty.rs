//! PTY wrapper — spawn bash under portable-pty, proxy I/O to/from the user's
//! real terminal, filter OSC 6973 out, feed a headless vt100 parser, and draw
//! ghost text after each refresh.

use crate::ansi;
use crate::history;
use crate::render::Renderer;
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
    let shell_path = find_bash()?;
    let init_path = materialize_shell_integration()?;

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
    cmd.args(["--init-file", init_path.to_str().unwrap()]);
    cmd.env("INSH_RS", "1");
    cmd.env("TERM", "xterm-256color");
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

    let registry = Registry::new_with_defaults();
    let hist = history::load();
    let engine = Engine::new(registry, hist);

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
    let mut renderer = Renderer::new(std::io::stdout());
    let mut pending_suggestion: Option<String> = None;

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let _ = status;
            break;
        }

        let mut made_progress = false;

        // Drain PTY output.
        while let Ok(bytes) = pty_rx.try_recv() {
            let (clean, events) = ansi::scan(&bytes);
            // Write clean bytes to real terminal.
            out.write_all(&clean).ok();
            out.flush().ok();
            tracker.feed(&clean, &events);
            made_progress = true;
        }

        // Drain user stdin and forward to PTY.
        while let Ok(bytes) = stdin_rx.try_recv() {
            // Intercept Right arrow (ESC [ C) or End (ESC [ F) to accept
            // the current suggestion by injecting its bytes into bash
            // before forwarding the keypress.
            let accept_keys: &[&[u8]] = &[b"\x1b[C", b"\x1b[F", b"\x05"];
            let is_accept = accept_keys.contains(&bytes.as_slice());
            if is_accept {
                if let Some(tail) = pending_suggestion.take() {
                    renderer.clear().ok();
                    writer.write_all(tail.as_bytes()).ok();
                    writer.flush().ok();
                    made_progress = true;
                    continue;
                }
            }
            // Clear any ghost text first — bash will echo the typed bytes
            // into the same cells.
            renderer.clear().ok();
            writer.write_all(&bytes).ok();
            writer.flush().ok();
            made_progress = true;
        }

        if made_progress {
            // After the new state is parsed, compute a suggestion and draw
            // ghost text.
            let state = tracker.state().clone();
            let cwd = tracker.cwd().to_string();
            if !state.command.is_empty() {
                let tail = engine.suggest(&state.command, &cwd);
                pending_suggestion = tail.clone();
                renderer.draw(tail.as_deref()).ok();
            } else {
                pending_suggestion = None;
                renderer.clear().ok();
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }

    terminal::disable_raw_mode().ok();
    let _ = crossterm::execute!(std::io::stdout(), terminal::Clear(ClearType::CurrentLine));
    Ok(())
}

fn find_bash() -> Result<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let candidate = std::path::Path::new(dir).join("bash");
        if candidate.exists() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("bash not found on PATH")
}

fn materialize_shell_integration() -> Result<std::path::PathBuf> {
    // Write the vendored shellIntegration.bash to a deterministic path so a
    // fresh PTY can --init-file it.
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("insh-rs");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("shellIntegration.bash");
    let contents = include_str!("../shell/shellIntegration.bash");
    std::fs::write(&path, contents)?;
    Ok(path)
}
