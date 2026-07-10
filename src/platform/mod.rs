//! Platform abstraction for PTY + terminal control.
//!
//! Two implementations:
//! - `unix.rs` (cfg(unix)) — POSIX forkpty/poll/termios. Linux + macOS.
//! - `windows.rs` (cfg(windows)) — ConPTY + WaitForMultipleObjects.
//!
//! `keys.rs` holds the console key-decoding logic the Windows reader uses. It
//! is compiled everywhere so it can be unit-tested off Windows.

pub mod keys;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Result type for platform operations.
pub type PtyResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Spawned PTY handle — wraps the platform-specific PTY resources.
/// The main loop in pty.rs uses this trait to read/write/poll/resize
/// without knowing which platform it's on.
pub trait PtyHandle {
    /// Raw fd (unix) or handle (windows) for the PTY output stream.
    /// Used by the main loop's poll/wait logic.
    fn output_fd(&self) -> RawDescriptor;

    /// Raw fd (unix) or handle (windows) for stdin.
    fn stdin_fd(&self) -> RawDescriptor;

    /// Write bytes to the PTY (sends to the child shell's stdin).
    fn pty_write(&self, data: &[u8]);

    /// Resize the PTY to new dimensions.
    fn resize(&self, rows: u16, cols: u16);

    /// Non-blocking check: has the child exited?
    fn try_wait(&mut self) -> Option<i32>;

    /// Block until data is available on output_fd or stdin_fd,
    /// or timeout_ms expires. Returns (pty_ready, stdin_ready).
    fn poll(&self, timeout_ms: i32) -> (bool, bool);

    /// Non-blocking read from the PTY output.
    fn read_pty(&self, buf: &mut [u8]) -> isize;

    /// Non-blocking read from stdin.
    fn read_stdin(&self, buf: &mut [u8]) -> isize;

    /// Close the PTY. Called on exit.
    fn close(&mut self);
}

/// Platform-specific raw descriptor type.
#[cfg(unix)]
pub type RawDescriptor = libc::c_int;

#[cfg(windows)]
pub type RawDescriptor = *mut std::ffi::c_void; // HANDLE

/// Query terminal dimensions.
pub fn term_size() -> Option<(u16, u16)> {
    #[cfg(unix)]
    {
        unix::term_size()
    }
    #[cfg(windows)]
    {
        windows::term_size()
    }
}

/// Enable raw mode on stdin. Returns a guard that restores on drop.
pub fn enable_raw_mode() {
    #[cfg(unix)]
    unix::enable_raw_mode();
    #[cfg(windows)]
    windows::enable_raw_mode();
}

/// Restore original terminal mode.
pub fn disable_raw_mode() {
    #[cfg(unix)]
    unix::disable_raw_mode();
    #[cfg(windows)]
    windows::disable_raw_mode();
}

/// Install signal/event handlers that restore terminal on crash.
pub fn install_signal_handlers() {
    #[cfg(unix)]
    unix::install_signal_handlers();
    #[cfg(windows)]
    windows::install_signal_handlers();
}

/// Find a binary on PATH (platform-aware separator + .exe suffix).
pub fn find_on_path(binary: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ';' } else { ':' };
    let suffixes: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in path.split(sep) {
        for suffix in suffixes {
            let candidate = std::path::Path::new(dir).join(format!("{}{}", binary, suffix));
            if candidate.exists() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}
