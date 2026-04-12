//! Windows PTY implementation — ConPTY (Windows 10 1809+).
//!
//! Uses CreatePseudoConsole + CreateProcessW to spawn a shell under
//! a pseudo-console, matching upstream's node-pty ConPTY integration.

#![cfg(windows)]

use super::{PtyHandle, PtyResult, RawDescriptor};
use std::mem;
use std::ptr;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;

pub struct WindowsPty {
    hpc: HPCON,
    child_process: HANDLE,
    child_thread: HANDLE,
    pty_input_write: HANDLE,
    pty_output_read: HANDLE,
    stdin_handle: HANDLE,
}

impl WindowsPty {
    pub fn spawn(
        bin: &str,
        argv: &[String],
        env: &[(String, String)],
        rows: u16,
        cols: u16,
    ) -> PtyResult<Self> {
        unsafe {
            // Create pipes for ConPTY I/O.
            let mut pty_input_read: HANDLE = INVALID_HANDLE_VALUE;
            let mut pty_input_write: HANDLE = INVALID_HANDLE_VALUE;
            let mut pty_output_read: HANDLE = INVALID_HANDLE_VALUE;
            let mut pty_output_write: HANDLE = INVALID_HANDLE_VALUE;

            if CreatePipe(&mut pty_input_read, &mut pty_input_write, ptr::null(), 0) == 0 {
                return Err("CreatePipe (input) failed".into());
            }
            if CreatePipe(&mut pty_output_read, &mut pty_output_write, ptr::null(), 0) == 0 {
                CloseHandle(pty_input_read);
                CloseHandle(pty_input_write);
                return Err("CreatePipe (output) failed".into());
            }

            // Create the pseudo-console.
            let size = COORD {
                X: cols as i16,
                Y: rows as i16,
            };
            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(
                size,
                pty_input_read,
                pty_output_write,
                0,
                &mut hpc,
            );
            if hr != 0 {
                CloseHandle(pty_input_read);
                CloseHandle(pty_input_write);
                CloseHandle(pty_output_read);
                CloseHandle(pty_output_write);
                return Err(format!("CreatePseudoConsole failed: HRESULT 0x{:08x}", hr).into());
            }

            // Close pipe ends the child will use — parent keeps
            // pty_input_write (→ child stdin) and pty_output_read (← child stdout).
            CloseHandle(pty_input_read);
            CloseHandle(pty_output_write);

            // Prepare the process attribute list with the ConPTY handle.
            let mut attr_size: usize = 0;
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_size);
            let attr_buf = vec![0u8; attr_size];
            let attr_list = attr_buf.as_ptr() as *mut LPPROC_THREAD_ATTRIBUTE_LIST;

            if InitializeProcThreadAttributeList(attr_list as _, 1, 0, &mut attr_size) == 0 {
                ClosePseudoConsole(hpc);
                CloseHandle(pty_input_write);
                CloseHandle(pty_output_read);
                return Err("InitializeProcThreadAttributeList failed".into());
            }

            if UpdateProcThreadAttribute(
                attr_list as _,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                hpc as *const _,
                mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null(),
            ) == 0
            {
                ClosePseudoConsole(hpc);
                CloseHandle(pty_input_write);
                CloseHandle(pty_output_read);
                return Err("UpdateProcThreadAttribute failed".into());
            }

            // Build the command line as a wide string.
            let cmd_line = if argv.len() > 1 {
                argv.join(" ")
            } else {
                bin.to_string()
            };
            let mut cmd_wide: Vec<u16> = cmd_line.encode_utf16().chain(std::iter::once(0)).collect();

            // Build environment block (null-separated, double-null terminated).
            let mut env_block: Vec<u16> = Vec::new();
            // Inherit parent env first.
            for (k, v) in std::env::vars() {
                let entry = format!("{}={}", k, v);
                env_block.extend(entry.encode_utf16());
                env_block.push(0);
            }
            // Override with our env vars.
            for (k, v) in env {
                let entry = format!("{}={}", k, v);
                env_block.extend(entry.encode_utf16());
                env_block.push(0);
            }
            env_block.push(0); // double-null terminator

            let mut si: STARTUPINFOEXW = mem::zeroed();
            si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
            si.lpAttributeList = attr_list as _;

            let mut pi: PROCESS_INFORMATION = mem::zeroed();

            let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;

            let ok = CreateProcessW(
                ptr::null(),
                cmd_wide.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                FALSE,
                flags,
                env_block.as_ptr() as *const _,
                ptr::null(),
                &si.StartupInfo as *const _ as *const STARTUPINFOW,
                &mut pi,
            );

            if ok == 0 {
                ClosePseudoConsole(hpc);
                CloseHandle(pty_input_write);
                CloseHandle(pty_output_read);
                return Err(format!(
                    "CreateProcessW failed: {}",
                    std::io::Error::last_os_error()
                )
                .into());
            }

            let stdin_handle = GetStdHandle(STD_INPUT_HANDLE);

            Ok(Self {
                hpc,
                child_process: pi.hProcess,
                child_thread: pi.hThread,
                pty_input_write,
                pty_output_read,
                stdin_handle,
            })
        }
    }
}

impl PtyHandle for WindowsPty {
    fn output_fd(&self) -> RawDescriptor {
        self.pty_output_read as _
    }

    fn stdin_fd(&self) -> RawDescriptor {
        self.stdin_handle as _
    }

    fn pty_write(&self, data: &[u8]) {
        let mut written: u32 = 0;
        unsafe {
            WriteFile(
                self.pty_input_write,
                data.as_ptr(),
                data.len() as u32,
                &mut written,
                ptr::null_mut(),
            );
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        unsafe { ResizePseudoConsole(self.hpc, size) };
    }

    fn try_wait(&mut self) -> Option<i32> {
        unsafe {
            let result = WaitForSingleObject(self.child_process, 0);
            if result == WAIT_OBJECT_0 {
                let mut exit_code: u32 = 0;
                GetExitCodeProcess(self.child_process, &mut exit_code);
                Some(exit_code as i32)
            } else {
                None
            }
        }
    }

    fn poll(&self, timeout_ms: i32) -> (bool, bool) {
        unsafe {
            let handles = [self.pty_output_read, self.stdin_handle];
            let result = WaitForMultipleObjects(
                2,
                handles.as_ptr(),
                FALSE,
                timeout_ms as u32,
            );
            match result {
                WAIT_OBJECT_0 => (true, false),
                v if v == WAIT_OBJECT_0 + 1 => (false, true),
                _ => (false, false),
            }
        }
    }

    fn read_pty(&self, buf: &mut [u8]) -> isize {
        let mut read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.pty_output_read,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 { -1 } else { read as isize }
    }

    fn read_stdin(&self, buf: &mut [u8]) -> isize {
        let mut read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.stdin_handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 { -1 } else { read as isize }
    }

    fn close(&mut self) {
        unsafe {
            ClosePseudoConsole(self.hpc);
            CloseHandle(self.child_process);
            CloseHandle(self.child_thread);
            CloseHandle(self.pty_input_write);
            CloseHandle(self.pty_output_read);
        }
    }
}

/// Saved original console mode for restore.
static mut ORIG_CONSOLE_MODE: u32 = 0;
static mut ORIG_MODE_SAVED: bool = false;

pub fn term_size() -> Option<(u16, u16)> {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
            let cols = (info.srWindow.Right - info.srWindow.Left + 1) as u16;
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as u16;
            if cols > 0 && rows > 0 {
                return Some((cols, rows));
            }
        }
        None
    }
}

pub fn enable_raw_mode() {
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        GetConsoleMode(h, std::ptr::addr_of_mut!(ORIG_CONSOLE_MODE));
        ORIG_MODE_SAVED = true;
        // Enable VT input processing, disable line input + echo.
        SetConsoleMode(
            h,
            ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT,
        );
        // Enable VT output on stdout.
        let hout = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut out_mode: u32 = 0;
        GetConsoleMode(hout, &mut out_mode);
        SetConsoleMode(
            hout,
            out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
        );
    }
}

pub fn disable_raw_mode() {
    unsafe {
        if ORIG_MODE_SAVED {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            SetConsoleMode(h, ORIG_CONSOLE_MODE);
        }
    }
}

pub fn install_signal_handlers() {
    // On Windows, Ctrl-C is handled by SetConsoleCtrlHandler.
    // ConPTY forwards it to the child process automatically.
    // We install a handler that restores console mode on exit.
    unsafe extern "system" fn handler(_ctrl_type: u32) -> BOOL {
        disable_raw_mode();
        FALSE // let default handler run (terminates process)
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), TRUE);
    }
}

/// Search known Git Bash installation paths (matches upstream's gitBashPath).
#[cfg(windows)]
pub fn find_git_bash() -> Option<String> {
    let candidates = [
        ("ProgramW6432", r"Git\bin\bash.exe"),
        ("ProgramW6432", r"Git\usr\bin\bash.exe"),
        ("ProgramFiles", r"Git\bin\bash.exe"),
        ("ProgramFiles(x86)", r"Git\bin\bash.exe"),
        ("LocalAppData", r"Programs\Git\bin\bash.exe"),
        ("UserProfile", r"scoop\apps\git\current\bin\bash.exe"),
        ("UserProfile", r"scoop\apps\git-with-openssh\current\bin\bash.exe"),
    ];
    for (env_var, suffix) in &candidates {
        if let Ok(base) = std::env::var(env_var) {
            let path = std::path::Path::new(&base).join(suffix);
            if path.exists() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }
    None
}
