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

/// Enable raw mode on stdin. Returns false when stdin is not a usable
/// terminal (no tty / no console handle); the caller treats that as fatal.
pub fn enable_raw_mode() -> bool {
    #[cfg(unix)]
    {
        unix::enable_raw_mode()
    }
    #[cfg(windows)]
    {
        windows::enable_raw_mode()
    }
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
///
/// Only a file the OS would actually run counts as a match: a directory or a
/// non-executable file earlier on PATH must not shadow the real binary further
/// down (it would be handed to execvp and fail with EACCES). Empty PATH
/// elements are skipped rather than resolved against the current directory.
pub fn find_on_path(binary: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    find_on_path_in(&path, binary)
}

/// `find_on_path` against an explicit PATH string, so it can be tested
/// without touching the process environment.
fn find_on_path_in(path_var: &str, binary: &str) -> Option<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.split(sep).filter(|dir| !dir.is_empty()) {
        for suffix in exe_suffixes() {
            let candidate = std::path::Path::new(dir).join(format!("{binary}{suffix}"));
            if is_executable_file(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Executable-name suffixes to try, most specific first. On Windows the bare
/// name comes last so an unrelated extensionless file cannot shadow `foo.exe`
/// (`CreateProcess` would not run it anyway); Unix names carry no suffix.
fn exe_suffixes() -> &'static [&'static str] {
    if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    }
}

/// Is `path` a regular file the OS would execute? Symlinks are followed, so a
/// `/usr/bin/python3 -> python3.12` chain still matches.
#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows has no execute bit; spawnability is decided by the file extension,
/// which [`exe_suffixes`] already constrains.
#[cfg(windows)]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A throwaway directory of PATH candidates, removed on drop.
    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new(tag: &str) -> Self {
            Self(crate::test_support::unique_temp_dir(&format!("path-{tag}")))
        }
        fn write_executable(&self, name: &str) {
            let path = self.0.join(name);
            fs::write(&path, b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn make_non_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// Join directories into a PATH string with the platform's separator.
    fn path_var(dirs: &[&std::path::Path]) -> String {
        let sep = if cfg!(windows) { ';' } else { ':' };
        dirs.iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(&sep.to_string())
    }

    /// A directory or a non-executable file earlier on PATH must not shadow
    /// the real binary further down (issue #60).
    #[test]
    fn only_an_executable_regular_file_is_a_match() {
        let dir = Sandbox::new("shadow");
        let real = Sandbox::new("real");
        real.write_executable("prog");

        // A directory named like the binary.
        fs::create_dir_all(dir.0.join("prog")).unwrap();
        assert_eq!(
            find_on_path_in(&path_var(&[&dir.0, &real.0]), "prog"),
            Some(real.0.join("prog").to_string_lossy().into_owned())
        );

        // A non-executable regular file named like the binary (Unix only:
        // Windows has no execute bit to check).
        #[cfg(unix)]
        {
            let plain_dir = Sandbox::new("plain");
            let plain = plain_dir.0.join("prog");
            fs::write(&plain, b"not a program").unwrap();
            make_non_executable(&plain);
            assert_eq!(
                find_on_path_in(&path_var(&[&plain_dir.0, &real.0]), "prog"),
                Some(real.0.join("prog").to_string_lossy().into_owned())
            );
        }

        // The executable candidate itself is still found.
        assert_eq!(
            find_on_path_in(&path_var(&[&real.0]), "prog"),
            Some(real.0.join("prog").to_string_lossy().into_owned())
        );
    }

    /// An empty PATH element means "the current directory" in POSIX — exactly
    /// the resolution an autocomplete wrapper must not perform.
    #[test]
    fn empty_path_elements_are_skipped() {
        let dir = Sandbox::new("empty");
        dir.write_executable("prog");
        // Leading empty element: nothing may be resolved relative to the CWD,
        // so only the absolute entry is consulted.
        let path = path_var(&[std::path::Path::new(""), &dir.0]);
        assert_eq!(
            find_on_path_in(&path, "prog"),
            Some(dir.0.join("prog").to_string_lossy().into_owned())
        );
        // Nothing but empty elements: no match, never a relative path.
        assert_eq!(
            find_on_path_in(&path_var(&[Path::new(""), Path::new("")]), "prog"),
            None
        );
        assert_eq!(find_on_path_in("", "prog"), None);
    }

    /// On Windows an extensionless file must be tried after `foo.exe`, or a
    /// stray download named `coreutils` disables coreutils detection.
    #[test]
    fn the_bare_name_is_the_last_windows_suffix() {
        let suffixes = exe_suffixes();
        if cfg!(windows) {
            assert_eq!(suffixes, [".exe", ".cmd", ".bat", ""]);
        } else {
            assert_eq!(suffixes, [""]);
        }
    }
}
