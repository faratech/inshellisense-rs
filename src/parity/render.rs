//! `render` parity category — drive both binaries' interactive popup
//! through a fixed PTY, capture output, replay through vt100, and diff
//! the resulting screen grids cell-by-cell.
//!
//! This is the most important category because rendering is the thing
//! the user visually compares.

use super::{Case, CaseResult, Category, CategoryReport, ScanConfig};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const PTY_ROWS: u16 = 40;
const PTY_COLS: u16 = 120;

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    keys: String,
    #[serde(default = "default_settle")]
    settle_ms: u64,
    /// Optional: send these additional keystrokes after the initial
    /// settle (e.g. to test escape-dismiss / tab-accept).
    #[serde(default)]
    then: Option<String>,
    #[serde(default)]
    then_settle_ms: Option<u64>,
}

fn default_settle() -> u64 {
    1500
}

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Render);
    let corpus_path = cfg.corpus_dir.join("render.jsonl");
    let scenarios = match load_corpus(&corpus_path) {
        Ok(s) => s,
        Err(_) => {
            report.summary = format!("no render corpus at {}", corpus_path.display());
            return report;
        }
    };
    let _ = fs::create_dir_all(&cfg.raw_dir);

    // Render scenarios run in parallel — each scenario spawns its
    // own isolated PTY + subprocess pair, so there's no shared state
    // to contend over. This roughly 8x's throughput on a typical
    // 8-core box.
    let cases: Vec<Case> = std::thread::scope(|s| {
        let handles: Vec<_> = scenarios
            .iter()
            .map(|scenario| s.spawn(|| {
            let ours_bytes = run_scenario(&cfg.ours, scenario);
            let upstream_bytes = run_scenario(&cfg.upstream, scenario);

            let ours_path = cfg.raw_dir.join(format!("render-{}-ours.bin", scenario.name));
            let upstream_path = cfg
                .raw_dir
                .join(format!("render-{}-upstream.bin", scenario.name));
            let _ = fs::write(&ours_path, &ours_bytes);
            let _ = fs::write(&upstream_path, &upstream_bytes);

            let ours_screen = replay(&ours_bytes);
            let up_screen = replay(&upstream_bytes);

            let diff = diff_screens(&up_screen, &ours_screen);
            if diff.is_empty() {
                Case {
                    name: scenario.name.clone(),
                    result: CaseResult::Pass,
                    impact: 0,
                }
            } else {
                let impact = if diff.len() > 50 { 80 } else { 60 };
                Case {
                    name: scenario.name.clone(),
                    result: CaseResult::Fail {
                        reason: format!(
                            "{} cells differ (bytes: ours={}, upstream={})",
                            diff.len(),
                            ours_bytes.len(),
                            upstream_bytes.len()
                        ),
                        details: diff.into_iter().take(10).collect(),
                    },
                    impact,
                }
            }
        }))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    report.cases = cases;

    report.summary = format!(
        "{}/{} render scenarios match",
        report.pass_count(),
        report.cases.len()
    );
    report
}

fn load_corpus(path: &Path) -> std::io::Result<Vec<Scenario>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(s) = serde_json::from_str::<Scenario>(line) {
            out.push(s);
        }
    }
    Ok(out)
}

fn run_scenario(bin: &Path, scenario: &Scenario) -> Vec<u8> {
    let mut master: libc::c_int = 0;
    let ws = libc::winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pid = unsafe {
        libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &ws)
    };
    match pid {
        -1 => return Vec::new(),
        0 => {
            // Child: exec the binary under test. Single-threaded post-fork,
            // pre-exec context, so mutating the environment is sound.
            unsafe {
                std::env::set_var("TERM", "xterm-256color");
                std::env::remove_var("COLORTERM");
                std::env::set_var("PS1", "$ ");
            }
            let c_bin = std::ffi::CString::new(bin.to_str().unwrap_or("")).unwrap();
            let args = ["start", "--ui", "popup"];
            let c_args: Vec<std::ffi::CString> = std::iter::once(c_bin.clone())
                .chain(args.iter().map(|a| std::ffi::CString::new(*a).unwrap()))
                .collect();
            let c_ptrs: Vec<*const libc::c_char> = c_args.iter()
                .map(|a| a.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            unsafe { libc::execvp(c_bin.as_ptr(), c_ptrs.as_ptr()) };
            unsafe { libc::_exit(127) };
        }
        _ => {}
    }

    let fd = master;

    let drain = |captured: &mut Vec<u8>, dur: Duration| {
        let end = Instant::now() + dur;
        let mut buf = [0u8; 4096];
        while Instant::now() < end {
            let mut fds = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }];
            let remaining = (end - Instant::now()).as_millis().min(50) as i32;
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, remaining) };
            if n > 0 && fds[0].revents & libc::POLLIN != 0 {
                let r = unsafe { libc::read(fd, buf.as_mut_ptr() as _, buf.len()) };
                if r > 0 {
                    captured.extend_from_slice(&buf[..r as usize]);
                }
            }
        }
    };

    let mut captured = Vec::new();
    drain(&mut captured, Duration::from_millis(5000));

    for ch in scenario.keys.chars() {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        unsafe { libc::write(fd, s.as_bytes().as_ptr() as _, s.len()) };
        drain(&mut captured, Duration::from_millis(800));
    }

    drain(&mut captured, Duration::from_millis(scenario.settle_ms));

    if let Some(then) = &scenario.then {
        for ch in then.chars() {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            unsafe { libc::write(fd, s.as_bytes().as_ptr() as _, s.len()) };
            drain(&mut captured, Duration::from_millis(150));
        }
        drain(&mut captured, Duration::from_millis(scenario.then_settle_ms.unwrap_or(500)));
    }

    unsafe { libc::write(fd, b"\x03".as_ptr() as _, 1) };
    drain(&mut captured, Duration::from_millis(200));
    unsafe { libc::write(fd, b"exit\r".as_ptr() as _, 5) };
    drain(&mut captured, Duration::from_millis(500));

    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
        libc::close(fd);
    }

    captured
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Screen {
    /// `rows` strings, each padded to `cols` display cells.
    rows: Vec<String>,
}

fn replay(raw: &[u8]) -> Screen {
    // Truncate at the first teardown byte: `\x1b c` (RIS — full
    // terminal reset) or `\x03` (Ctrl-C) that occurs AFTER the last
    // popup draw. These cleanup sequences wipe the screen state in
    // the vt100 parser, erasing the popup we want to inspect.
    let truncated = truncate_before_teardown(raw);
    // Strip OSC 6973 markers via crate::ansi::scan, then feed to vt100.
    let (clean, _events) = crate::ansi::scan(truncated);
    // Also strip bracketed paste toggles so they don't confuse vt100.
    let clean = strip_sequences(&clean, &[b"\x1b[?2004h", b"\x1b[?2004l"]);
    let mut parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, 0);
    parser.process(&clean);
    let screen = parser.screen();
    let mut rows = Vec::with_capacity(PTY_ROWS as usize);
    for r in 0..PTY_ROWS {
        let mut row = String::new();
        for c in 0..PTY_COLS {
            if let Some(cell) = screen.cell(r, c) {
                let contents = cell.contents();
                if contents.is_empty() {
                    row.push(' ');
                } else {
                    row.push_str(contents);
                }
            } else {
                row.push(' ');
            }
        }
        // Trim trailing spaces so blank rows are cheap to compare.
        let trimmed = row.trim_end().to_string();
        rows.push(trimmed);
    }
    Screen { rows }
}

/// Find the last "popup draw" cycle (as opposed to clear cycles or
/// teardown) and truncate the stream so the vt100 replay sees it as
/// the final screen state. Strategy:
///
/// 1. Walk all `\x1b[s` (SCO save cursor) positions — each marks the
///    start of a save/restore draw or clear cycle.
/// 2. For each save, find its matching `\x1b[u` (restore) and check
///    whether the content between contains a box-drawing char
///    (`┌` U+250C). If yes, that cycle is a DRAW. If no, it's a
///    CLEAR (or an empty cycle).
/// 3. Take the last DRAW cycle's end position as the truncation
///    boundary.
///
/// Without this, teardown clear cycles erase the popup in the
/// parser's view. Falls back to slicing at the first teardown marker
/// (`\x03` / `\x1bc`) when no draw cycle is found.
fn truncate_before_teardown(raw: &[u8]) -> &[u8] {
    let save = b"\x1b[s";
    let restore = b"\x1b[u";
    let box_char: &[u8] = &[0xe2, 0x94, 0x8c];

    let mut last_draw_end: Option<usize> = None;
    let mut pos = 0;
    while pos < raw.len() {
        let Some(save_rel) = find_first_slice(&raw[pos..], save) else {
            break;
        };
        let save_abs = pos + save_rel;
        let cycle_start = save_abs + save.len();
        let Some(restore_rel) = find_first_slice(&raw[cycle_start..], restore) else {
            break;
        };
        let cycle_end = cycle_start + restore_rel + restore.len();
        let cycle = &raw[cycle_start..cycle_end];
        if find_first_slice(cycle, box_char).is_some() {
            last_draw_end = Some(cycle_end);
        }
        pos = cycle_end;
    }

    if let Some(end) = last_draw_end {
        let end = (end + 8).min(raw.len());
        return &raw[..end];
    }
    // Fallback: slice before first Ctrl-C or RIS.
    for (i, &b) in raw.iter().enumerate() {
        if b == 0x03 {
            return &raw[..i];
        }
        if b == 0x1b && raw.get(i + 1) == Some(&b'c') {
            return &raw[..i];
        }
    }
    raw
}

fn find_first_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    for i in 0..=haystack.len() - needle.len() {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}


fn strip_sequences(bytes: &[u8], needles: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut matched = false;
        for n in needles {
            if i + n.len() <= bytes.len() && &bytes[i..i + n.len()] == *n {
                i += n.len();
                matched = true;
                break;
            }
        }
        if !matched {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn diff_screens(upstream: &Screen, ours: &Screen) -> Vec<String> {
    let mut out = Vec::new();
    let max = upstream.rows.len().max(ours.rows.len());
    for i in 0..max {
        let u = upstream.rows.get(i).cloned().unwrap_or_default();
        let o = ours.rows.get(i).cloned().unwrap_or_default();
        if u != o {
            out.push(format!("row {}: upstream={:?}", i, u));
            out.push(format!("row {}: ours    ={:?}", i, o));
        }
    }
    out
}
