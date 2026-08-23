//! POSIX PTY implementation — Linux + macOS.
//!
//! Uses forkpty(3), poll(2), termios, waitpid(2). All POSIX-standard
//! syscalls available on any Unix.

use super::{PtyHandle, PtyResult, RawDescriptor};

/// Saved original termios for restore on exit/signal.
static mut ORIG_TERMIOS: libc::termios = unsafe { std::mem::zeroed() };
static mut ORIG_TERMIOS_SAVED: bool = false;

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub struct UnixPty {
    master_fd: libc::c_int,
    child_pid: libc::pid_t,
    /// Set once stdin has hit EOF/HUP/error. The event loop must then stop
    /// polling it — poll(2) reports a closed fd as permanently ready, which
    /// otherwise busy-spins the loop at 100% CPU with no exit path (#61).
    stdin_dead: std::cell::Cell<bool>,
}

impl UnixPty {
    pub fn spawn(
        bin: &str,
        argv: &[String],
        env: &[(String, String)],
        rows: u16,
        cols: u16,
    ) -> PtyResult<Self> {
        let mut master: libc::c_int = 0;
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pid =
            unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &ws) };
        match pid {
            -1 => Err(format!("forkpty failed: {}", std::io::Error::last_os_error()).into()),
            0 => {
                // Child: set env vars, then exec. Single-threaded post-fork,
                // pre-exec context, so mutating the environment is sound.
                for (k, v) in env {
                    unsafe {
                        std::env::set_var(k, v);
                    }
                }
                let c_bin = std::ffi::CString::new(bin.as_bytes()).expect("CString");
                let c_argv: Vec<std::ffi::CString> = argv
                    .iter()
                    .map(|a| std::ffi::CString::new(a.as_bytes()).expect("CString"))
                    .collect();
                let c_ptrs: Vec<*const libc::c_char> = c_argv
                    .iter()
                    .map(|a| a.as_ptr())
                    .chain(std::iter::once(std::ptr::null()))
                    .collect();
                unsafe { libc::execvp(c_bin.as_ptr(), c_ptrs.as_ptr()) };
                eprintln!("is: execvp failed: {}", std::io::Error::last_os_error());
                unsafe { libc::_exit(127) };
            }
            _ => {
                // Non-blocking master: the post-exit drain loop reads until
                // EAGAIN/EOF instead of blocking forever on a slave fd some
                // grandchild still holds open (#52). The main loop is
                // poll-gated, so this changes nothing for it.
                let flags = unsafe { libc::fcntl(master, libc::F_GETFL, 0) };
                if flags >= 0 {
                    unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) };
                }
                Ok(Self {
                    master_fd: master,
                    child_pid: pid,
                    stdin_dead: std::cell::Cell::new(false),
                })
            }
        }
    }
}

impl PtyHandle for UnixPty {
    fn output_fd(&self) -> RawDescriptor {
        self.master_fd
    }

    fn stdin_fd(&self) -> RawDescriptor {
        libc::STDIN_FILENO
    }

    fn pty_write(&self, data: &[u8]) {
        let mut offset = 0;
        while offset < data.len() {
            let n = unsafe {
                libc::write(
                    self.master_fd,
                    data[offset..].as_ptr() as *const libc::c_void,
                    data.len() - offset,
                )
            };
            if n <= 0 {
                break;
            }
            offset += n as usize;
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe { libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws) };
    }

    /// Returns the child's *exit code*, matching the Windows implementation.
    /// Returning the raw `waitpid` status here meant callers could not use the
    /// value, so the wrapped shell's exit status was silently discarded.
    fn try_wait(&mut self) -> Option<i32> {
        let mut status: libc::c_int = 0;
        let w = unsafe { libc::waitpid(self.child_pid, &mut status, libc::WNOHANG) };
        if w > 0 {
            Some(exit_code_from_status(status))
        } else {
            None
        }
    }

    fn poll(&self, timeout_ms: i32) -> (bool, bool) {
        let mut fds = [
            libc::pollfd {
                fd: self.master_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds = if self.stdin_dead.get() { 1 } else { 2 };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), nfds, timeout_ms) };
        if n <= 0 {
            return (false, false);
        }
        // HUP/ERR/NVAL on stdin mean it will never deliver more input; stop
        // polling it or poll(2) reports it ready every cycle (#61).
        if nfds == 2 && fds[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            self.stdin_dead.set(true);
        }
        (
            fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0,
            !self.stdin_dead.get() && fds[1].revents & libc::POLLIN != 0,
        )
    }

    fn read_pty(&self, buf: &mut [u8]) -> isize {
        unsafe {
            libc::read(
                self.master_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        }
    }

    fn read_stdin(&self, buf: &mut [u8]) -> isize {
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n == 0 || (n < 0 && !matches!(errno(), libc::EINTR | libc::EAGAIN)) {
            // EOF or unrecoverable error: never poll this fd again (#61).
            self.stdin_dead.set(true);
        }
        n
    }

    fn close(&mut self) {
        unsafe { libc::close(self.master_fd) };
    }
}

pub fn term_size() -> Option<(u16, u16)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            Some((ws.ws_col, ws.ws_row))
        } else {
            None
        }
    }
}

/// Enable raw mode on stdin. Returns false when stdin is not a terminal —
/// the caller treats that as fatal, since without raw mode the tool cannot
/// observe keystrokes (and cooked-mode echo corrupts the display) (#61).
pub fn enable_raw_mode() -> bool {
    unsafe {
        let p = std::ptr::addr_of_mut!(ORIG_TERMIOS);
        if libc::tcgetattr(libc::STDIN_FILENO, p) != 0 {
            return false;
        }
        ORIG_TERMIOS_SAVED = true;
        let mut raw = *p;
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) == 0
    }
}

pub fn disable_raw_mode() {
    unsafe {
        if ORIG_TERMIOS_SAVED {
            libc::tcsetattr(
                libc::STDIN_FILENO,
                libc::TCSANOW,
                std::ptr::addr_of!(ORIG_TERMIOS),
            );
        }
    }
}

pub fn install_signal_handlers() {
    extern "C" fn handler(sig: libc::c_int) {
        unsafe {
            if ORIG_TERMIOS_SAVED {
                libc::tcsetattr(
                    libc::STDIN_FILENO,
                    libc::TCSANOW,
                    std::ptr::addr_of!(ORIG_TERMIOS),
                );
            }
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
}

/// Decode a `waitpid` status into the code a shell reports: the exit status,
/// or `128 + signal` when the child was killed by a signal.
fn exit_code_from_status(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        0
    }
}

#[cfg(test)]
mod exit_status_tests {
    use super::exit_code_from_status;

    #[test]
    fn decodes_normal_exit() {
        // `exit 42` → WIFEXITED, code 42.
        assert_eq!(exit_code_from_status(42 << 8), 42);
        assert_eq!(exit_code_from_status(0), 0);
    }

    #[test]
    fn decodes_signal_death() {
        // Killed by SIGKILL (9) → 128 + 9.
        assert_eq!(exit_code_from_status(libc::SIGKILL), 137);
    }
}
