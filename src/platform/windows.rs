//! Windows PTY implementation — ConPTY (Windows 10 1809+).
//!
//! Uses CreatePseudoConsole + CreateProcessW to spawn a shell under
//! a pseudo-console, matching upstream's node-pty ConPTY integration.

#![cfg(windows)]

use super::keys::{Modifiers, decode_utf16_unit, vkey_sequence};
use super::keys::{VK_BACK, VK_ESCAPE, VK_RETURN, VK_TAB};
use super::{PtyHandle, PtyResult, RawDescriptor};
use std::mem;
use std::ptr;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;

/// Releases a process thread attribute list on drop.
///
/// `InitializeProcThreadAttributeList` allocates internal structures that
/// MSDN requires be released with `DeleteProcThreadAttributeList` before the
/// backing memory is freed. Nothing called it, so every spawn leaked — on the
/// success path and on both failure paths after initialization succeeded
/// (#75).
struct AttrListGuard {
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl Drop for AttrListGuard {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

pub struct WindowsPty {
    hpc: HPCON,
    child_process: HANDLE,
    child_thread: HANDLE,
    pty_input_write: HANDLE,
    pty_output_read: HANDLE,
    stdin_handle: HANDLE,
    /// High half of a UTF-16 surrogate pair, awaiting its low half.
    pending_surrogate: std::cell::Cell<u16>,
}

impl WindowsPty {
    pub fn spawn(
        _bin: &str,
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
            let hr = CreatePseudoConsole(size, pty_input_read, pty_output_write, 0, &mut hpc);
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
            let attr_list: LPPROC_THREAD_ATTRIBUTE_LIST = attr_buf.as_ptr() as _;

            if InitializeProcThreadAttributeList(attr_list as _, 1, 0, &mut attr_size) == 0 {
                ClosePseudoConsole(hpc);
                CloseHandle(pty_input_write);
                CloseHandle(pty_output_read);
                return Err("InitializeProcThreadAttributeList failed".into());
            }

            // Initialized: the internals must be deleted before `attr_buf`
            // drops. Declared after it, so the guard drops first on every
            // path out of this scope (both error returns and success).
            let _attr_guard = AttrListGuard { list: attr_list };

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

            // Build the command line as a wide string, quoting any
            // arguments that contain spaces or quotes.
            let cmd_line = argv
                .iter()
                .map(|arg| {
                    if arg.contains(' ') || arg.contains('"') {
                        format!("\"{}\"", arg.replace('"', "\\\""))
                    } else {
                        arg.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let mut cmd_wide: Vec<u16> =
                cmd_line.encode_utf16().chain(std::iter::once(0)).collect();

            // Build environment block (null-separated, double-null terminated).
            // Windows uses the FIRST occurrence of a duplicate key, so
            // overrides must come before inherited vars. We use a BTreeMap
            // with uppercased keys (Windows env vars are case-insensitive)
            // to deduplicate, and the sorted output satisfies Windows'
            // expectation of a sorted environment block.
            let mut env_map = std::collections::BTreeMap::<String, (String, String)>::new();
            // Overrides first — these must win.
            for (k, v) in env {
                env_map.insert(k.to_uppercase(), (k.clone(), v.clone()));
            }
            // Inherit parent vars only if not already overridden.
            for (k, v) in std::env::vars() {
                env_map.entry(k.to_uppercase()).or_insert((k, v));
            }
            let mut env_block: Vec<u16> = Vec::new();
            for (_upper, (k, v)) in &env_map {
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
                return Err(
                    format!("CreateProcessW failed: {}", std::io::Error::last_os_error()).into(),
                );
            }

            let stdin_handle = GetStdHandle(STD_INPUT_HANDLE);

            Ok(Self {
                hpc,
                child_process: pi.hProcess,
                child_thread: pi.hThread,
                pty_input_write,
                pty_output_read,
                stdin_handle,
                pending_surrogate: std::cell::Cell::new(0),
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
            // Loop like the Unix implementation: `WriteFile` may report a
            // short write on a full pipe, and its failure was ignored
            // entirely, so the remainder of a keystroke burst was dropped.
            let mut offset = 0usize;
            while offset < data.len() {
                let ok = WriteFile(
                    self.pty_input_write,
                    data[offset..].as_ptr(),
                    (data.len() - offset) as u32,
                    &mut written,
                    ptr::null_mut(),
                );
                if ok == 0 || written == 0 {
                    break;
                }
                offset += written as usize;
            }
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
            // WaitForMultipleObjects with bWaitAll=FALSE only reports the
            // lowest-index signaled handle. Probe the other handle with a
            // zero-timeout WaitForSingleObject so both are reported.
            //
            // Console input handles are signaled when ANY event (key,
            // mouse, focus, resize) is queued — read_stdin() filters for
            // key-down events via ReadConsoleInputW.
            let handles = [self.pty_output_read, self.stdin_handle];
            let result = WaitForMultipleObjects(2, handles.as_ptr(), FALSE, timeout_ms as u32);
            match result {
                WAIT_OBJECT_0 => {
                    let stdin_also = WaitForSingleObject(self.stdin_handle, 0) == WAIT_OBJECT_0;
                    (true, stdin_also)
                }
                v if v == WAIT_OBJECT_0 + 1 => {
                    let pty_also = WaitForSingleObject(self.pty_output_read, 0) == WAIT_OBJECT_0;
                    (pty_also, true)
                }
                _ => (false, false),
            }
        }
    }

    fn read_pty(&self, buf: &mut [u8]) -> isize {
        unsafe {
            // ConPTY signals the pipe handle even when no data is
            // pending, so ReadFile after poll() can still block.
            // Use PeekNamedPipe as a gate, but read the full buffer
            // size (not just `avail`) so we get larger chunks and
            // avoid splitting OSC 6973 sequences across reads.
            let mut avail: u32 = 0;
            if PeekNamedPipe(
                self.pty_output_read,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut avail,
                ptr::null_mut(),
            ) == 0
            {
                return -1; // pipe broken
            }
            if avail == 0 {
                return 0;
            }
            let mut read: u32 = 0;
            let ok = ReadFile(
                self.pty_output_read,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                ptr::null_mut(),
            );
            if ok == 0 { -1 } else { read as isize }
        }
    }

    fn read_stdin(&self, buf: &mut [u8]) -> isize {
        unsafe {
            // Read console input events directly — no relay thread/pipe.
            // ReadConsoleInputW returns individual INPUT_RECORD events
            // regardless of console mode, so it never blocks on line
            // input. Non-key events are consumed and discarded.
            let mut total: usize = 0;
            loop {
                // Stop if we'd overflow the buffer.
                if total + 16 > buf.len() {
                    break;
                }
                // Check for pending events before reading.
                let mut pending: u32 = 0;
                if GetNumberOfConsoleInputEvents(self.stdin_handle, &mut pending) == 0
                    || pending == 0
                {
                    break;
                }
                let mut rec: INPUT_RECORD = mem::zeroed();
                let mut num_read: u32 = 0;
                if ReadConsoleInputW(self.stdin_handle, &mut rec, 1, &mut num_read) == 0
                    || num_read == 0
                {
                    break;
                }
                // Only process key-down events.
                if rec.EventType != KEY_EVENT as u16 {
                    continue;
                }
                let key = rec.Event.KeyEvent;
                if key.bKeyDown == 0 {
                    continue;
                }
                // Skip synthetic VT sequence chars injected by
                // ENABLE_VIRTUAL_TERMINAL_INPUT (Windows Terminal's
                // default). These have vk=0 and are terminal-generated
                // noise, not real user keystrokes.
                if key.wVirtualKeyCode == 0 {
                    continue;
                }

                let mods = Modifiers::from_control_key_state(key.dwControlKeyState);
                let ch = key.uChar.UnicodeChar;

                // Keys like Backspace, Tab, Return have a non-zero
                // UnicodeChar (0x08, 0x09, 0x0D) but need specific byte
                // values, so resolve them by virtual key first.
                let handled_by_vkey = matches!(
                    key.wVirtualKeyCode,
                    VK_BACK | VK_TAB | VK_RETURN | VK_ESCAPE
                );

                // Windows collapses auto-repeat into one record. Emitting the
                // key once dropped every repeat but the first.
                let repeat = key.wRepeatCount.max(1) as usize;

                let mut scratch = [0u8; 8];
                let mut utf8 = [0u8; 4];
                let bytes: &[u8] = if ch != 0 && !handled_by_vkey {
                    let Some(scalar) = decode_utf16_unit(&self.pending_surrogate, ch) else {
                        continue;
                    };
                    let len = scalar.encode_utf8(&mut utf8).len();
                    &utf8[..len]
                } else {
                    match vkey_sequence(key.wVirtualKeyCode, mods, &mut scratch) {
                        Some(seq) => seq,
                        None => continue,
                    }
                };

                for _ in 0..repeat {
                    if total + bytes.len() > buf.len() {
                        break;
                    }
                    buf[total..total + bytes.len()].copy_from_slice(bytes);
                    total += bytes.len();
                }
            }
            if total == 0 { -1 } else { total as isize }
        }
    }

    fn close(&mut self) {
        unsafe {
            // node-pty PR #415 two-thread shutdown: ClosePseudoConsole
            // emits a final frame on the output pipe and blocks until
            // it's consumed. A drain thread must read concurrently.
            //
            // 1. Close input pipe (signals end of input).
            // 2. Spawn drain thread on the output pipe.
            // 3. Call ClosePseudoConsole on this thread — the drain
            //    thread consumes the final frame so it can return.
            // 4. Drain thread hits EOF and exits.
            // 5. Close remaining handles.
            CloseHandle(self.pty_input_write);

            let h_output = self.pty_output_read as isize;
            let drain = std::thread::spawn(move || {
                let h = h_output as HANDLE;
                let mut buf = [0u8; 4096];
                loop {
                    let mut n: u32 = 0;
                    let ok = ReadFile(
                        h,
                        buf.as_mut_ptr(),
                        buf.len() as u32,
                        &mut n,
                        ptr::null_mut(),
                    );
                    if ok == 0 || n == 0 {
                        break;
                    }
                }
            });

            ClosePseudoConsole(self.hpc);
            let _ = drain.join();

            CloseHandle(self.pty_output_read);
            CloseHandle(self.child_process);
            CloseHandle(self.child_thread);
        }
    }
}

/// Saved original console modes for restore.
static mut ORIG_INPUT_MODE: u32 = 0;
static mut ORIG_OUTPUT_MODE: u32 = 0;
static mut INPUT_MODE_SAVED: bool = false;
static mut OUTPUT_MODE_SAVED: bool = false;

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
        // Save and modify input and output INDEPENDENTLY — in Git
        // Bash (MSYS2), stdout may be a pipe so GetConsoleMode fails
        // on it. We must still change the input mode to disable echo
        // and VT input artifacts.
        let h_in = GetStdHandle(STD_INPUT_HANDLE);
        if GetConsoleMode(h_in, std::ptr::addr_of_mut!(ORIG_INPUT_MODE)) != 0 {
            INPUT_MODE_SAVED = true;
            SetConsoleMode(h_in, ENABLE_WINDOW_INPUT);
        }
        let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
        if GetConsoleMode(h_out, std::ptr::addr_of_mut!(ORIG_OUTPUT_MODE)) != 0 {
            OUTPUT_MODE_SAVED = true;
            // Enable VT output (needed for powershell.exe). Do NOT
            // set DISABLE_NEWLINE_AUTO_RETURN — causes flash on exit.
            SetConsoleMode(h_out, ORIG_OUTPUT_MODE | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

pub fn disable_raw_mode() {
    unsafe {
        if INPUT_MODE_SAVED {
            let h_in = GetStdHandle(STD_INPUT_HANDLE);
            SetConsoleMode(h_in, ORIG_INPUT_MODE);
        }
        if OUTPUT_MODE_SAVED {
            let h_out = GetStdHandle(STD_OUTPUT_HANDLE);
            SetConsoleMode(h_out, ORIG_OUTPUT_MODE);
        }
    }
}

pub fn install_signal_handlers() {
    // On Windows, Ctrl-C is handled by SetConsoleCtrlHandler.
    // With ENABLE_PROCESSED_INPUT disabled it normally arrives as a key event
    // and is forwarded through ConPTY. If Windows still delivers a console
    // control event, consume Ctrl-C/Ctrl-Break so the wrapper does not die
    // before the child shell can handle the interrupt.
    unsafe extern "system" fn handler(ctrl_type: u32) -> windows_sys::core::BOOL {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => TRUE,
            _ => {
                disable_raw_mode();
                FALSE
            }
        }
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
        (
            "UserProfile",
            r"scoop\apps\git-with-openssh\current\bin\bash.exe",
        ),
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
