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
    // Zero scenarios would score a vacuous 100% — fail instead.
    let scenarios = match load_corpus(&corpus_path) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            report.push_fail(
                "corpus",
                format!(
                    "render corpus at {} has no scenarios",
                    corpus_path.display()
                ),
                Vec::new(),
                3,
            );
            return report;
        }
        Err(e) => {
            report.push_fail(
                "corpus",
                format!(
                    "cannot read render corpus at {}: {e}",
                    corpus_path.display()
                ),
                Vec::new(),
                3,
            );
            return report;
        }
    };
    let _ = fs::create_dir_all(&cfg.raw_dir);
    // Both binaries must run against the same scratch HOME and cwd, or the
    // capture reflects the developer's installed shells and config rather
    // than the implementations under test.
    let ours_home = super::isolated_home_ours(cfg);
    let upstream_home = super::isolated_home_upstream(cfg);

    // Render scenarios run in parallel — each scenario spawns its
    // own isolated PTY + subprocess pair, so there's no shared state
    // to contend over. This roughly 8x's throughput on a typical
    // 8-core box.
    let cases: Vec<Case> = std::thread::scope(|s| {
        let handles: Vec<_> = scenarios
            .iter()
            .map(|scenario| {
                s.spawn(|| {
                    let ours = run_scenario(&cfg.ours, scenario, &ours_home, true);
                    let upstream = run_scenario(&cfg.upstream, scenario, &upstream_home, false);

                    // Raw captures are written even for failed runs so a
                    // broken scenario can be replayed by hand.
                    if let Ok(bytes) = &ours {
                        let _ = fs::write(
                            cfg.raw_dir
                                .join(format!("render-{}-ours.bin", scenario.name)),
                            bytes,
                        );
                    }
                    if let Ok(bytes) = &upstream {
                        let _ = fs::write(
                            cfg.raw_dir
                                .join(format!("render-{}-upstream.bin", scenario.name)),
                            bytes,
                        );
                    }

                    evaluate_capture(scenario.name.clone(), ours, upstream)
                })
            })
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

/// Turn two PTY captures into a case result.
///
/// A failed capture used to be recorded as an empty byte stream, which
/// replayed to a blank screen on BOTH sides — the diff found nothing and a
/// PTY-broken environment (forkpty failures, an exec that never happened)
/// scored 100% render parity. Failures are reported as failures now, and so
/// is the "nothing at all reached the terminal" case, which cannot be a
/// real capture of a TUI.
fn evaluate_capture(
    name: String,
    ours: Result<Vec<u8>, String>,
    upstream: Result<Vec<u8>, String>,
) -> Case {
    let (ours_bytes, upstream_bytes) = match (ours, upstream) {
        (Ok(o), Ok(u)) => (o, u),
        (ours, upstream) => {
            let mut reasons = Vec::new();
            for (side, result) in [("ours", ours), ("upstream", upstream)] {
                if let Err(e) = result {
                    reasons.push(format!("{side}: {e}"));
                }
            }
            return Case {
                name,
                result: CaseResult::Fail {
                    reason: format!("capture failed — {}", reasons.join("; ")),
                    details: reasons,
                },
                impact: 90,
            };
        }
    };
    if ours_bytes.is_empty() && upstream_bytes.is_empty() {
        return Case {
            name,
            result: CaseResult::Fail {
                reason: "both captures are empty — nothing reached the terminal".to_string(),
                details: Vec::new(),
            },
            impact: 90,
        };
    }

    let ours_screen = replay(&ours_bytes);
    let up_screen = replay(&upstream_bytes);

    let diff = diff_screens(&up_screen, &ours_screen);
    if diff.is_empty() {
        Case {
            name,
            result: CaseResult::Pass,
            impact: 0,
        }
    } else {
        let impact = if diff.len() > 50 { 80 } else { 60 };
        Case {
            name,
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
}

/// A malformed JSONL line is a corpus regression, not a line to skip.
fn load_corpus(path: &Path) -> std::io::Result<Vec<Scenario>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<Scenario>(line) {
            Ok(scenario) => out.push(scenario),
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}:{}: {e}", path.display(), idx + 1),
                ));
            }
        }
    }
    Ok(out)
}

/// Capture one scenario through a PTY, or `Err` when the capture is
/// unusable. Returning an empty byte stream on failure used to replay to a
/// blank screen — which diffed equal to another blank screen and scored
/// perfect parity for a broken PTY environment.
fn run_scenario(
    bin: &Path,
    scenario: &Scenario,
    home: &Path,
    is_ours: bool,
) -> Result<Vec<u8>, String> {
    // Build everything the child needs BEFORE forking. Scenarios run on
    // parallel scope threads, so at the fork instant sibling threads may
    // hold allocator or environment locks; the child therefore does nothing
    // but async-signal-safe calls (chdir/execve/_exit). Allocating or
    // calling `std::env` post-fork could deadlock it before exec — another
    // way to produce an empty, vacuously-passing capture.
    let c_bin = std::ffi::CString::new(bin.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("binary path {} contains a NUL byte", bin.display()))?;
    let c_home = std::ffi::CString::new(home.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("home path {} contains a NUL byte", home.display()))?;
    let args: Vec<&str> = if is_ours {
        vec!["start", "--ui", "popup"]
    } else {
        vec!["-s", "bash"]
    };
    let c_args: Vec<std::ffi::CString> = std::iter::once(c_bin.clone())
        .chain(args.iter().map(|a| std::ffi::CString::new(*a).unwrap()))
        .collect();
    let argv: Vec<*const libc::c_char> = c_args
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp_cstrings = child_envp(home);
    let envp: Vec<*const libc::c_char> = envp_cstrings
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let mut master: libc::c_int = 0;
    let ws = libc::winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pid =
        unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &ws) };
    match pid {
        -1 => {
            return Err(format!(
                "forkpty failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        0 => unsafe {
            // Child of a multithreaded parent: async-signal-safe calls only.
            libc::chdir(c_home.as_ptr());
            libc::execve(c_bin.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        },
        _ => {}
    }

    let fd = master;

    let drain = |captured: &mut Vec<u8>, dur: Duration| {
        let end = Instant::now() + dur;
        let mut buf = [0u8; 4096];
        while Instant::now() < end {
            let mut fds = [libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            }];
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
        drain(
            &mut captured,
            Duration::from_millis(scenario.then_settle_ms.unwrap_or(500)),
        );
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

    Ok(captured)
}

/// The child's environment, built in the parent before forking.
///
/// Same contract as [`super::deterministic`]: machine-specific inputs
/// (INSH_RS*, ISTERM*, ZDOTDIR — so no operator spec sources and no host
/// coreutils probe on our side only) and COLORTERM are dropped; TERM, PS1,
/// HOME, and XDG_CONFIG_HOME are pinned.
fn child_envp(home: &Path) -> Vec<std::ffi::CString> {
    child_envp_from(std::env::vars_os(), home)
}

/// Same, from an explicit base environment so tests need not touch the
/// process-global one.
fn child_envp_from<I>(base: I, home: &Path) -> Vec<std::ffi::CString>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    fn push(out: &mut Vec<std::ffi::CString>, key: &std::ffi::OsStr, value: &std::ffi::OsStr) {
        let mut joined = key.as_encoded_bytes().to_vec();
        joined.push(b'=');
        joined.extend_from_slice(value.as_encoded_bytes());
        if let Ok(c) = std::ffi::CString::new(joined) {
            out.push(c);
        }
    }

    let mut out = Vec::new();
    for (k, v) in base {
        if super::is_machine_specific(&k.to_string_lossy())
            || k == *std::ffi::OsStr::new("COLORTERM")
            || k == *std::ffi::OsStr::new("HOME")
            || k == *std::ffi::OsStr::new("USERPROFILE")
            || k == *std::ffi::OsStr::new("XDG_CONFIG_HOME")
            || k == *std::ffi::OsStr::new("TERM")
            || k == *std::ffi::OsStr::new("PS1")
        {
            continue;
        }
        push(&mut out, &k, &v);
    }
    push(
        &mut out,
        std::ffi::OsStr::new("TERM"),
        std::ffi::OsStr::new("xterm-256color"),
    );
    push(
        &mut out,
        std::ffi::OsStr::new("PS1"),
        std::ffi::OsStr::new("$ "),
    );
    push(&mut out, std::ffi::OsStr::new("HOME"), home.as_os_str());
    push(
        &mut out,
        std::ffi::OsStr::new("XDG_CONFIG_HOME"),
        home.join(".config").as_os_str(),
    );
    push(
        &mut out,
        std::ffi::OsStr::new("INSH_RS_NO_COREUTILS"),
        std::ffi::OsStr::new("1"),
    );
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(bytes: &[u8]) -> Result<Vec<u8>, String> {
        Ok(bytes.to_vec())
    }

    /// A PTY-broken environment used to record perfect parity: forkpty
    /// failures became empty captures, which replayed to identical blank
    /// screens on both sides.
    #[test]
    fn forkpty_failure_fails_the_case() {
        let case = evaluate_capture(
            "single-c".to_string(),
            Err("forkpty failed: Too many open files".to_string()),
            Err("forkpty failed: Too many open files".to_string()),
        );
        let CaseResult::Fail { reason, .. } = case.result else {
            panic!("expected a failure, got {:?}", case.result);
        };
        assert!(reason.contains("capture failed"), "{reason}");
        assert!(reason.contains("ours"), "{reason}");
        assert!(reason.contains("upstream"), "{reason}");
        assert!(case.impact >= 90);
    }

    /// One-sided failure is a divergence too — it used to diff as
    /// "blank vs content" noise instead of naming the broken side.
    #[test]
    fn one_sided_failure_names_the_side() {
        let case = evaluate_capture(
            "single-c".to_string(),
            capture(b"$ prompt"),
            Err("forkpty failed: no ptmx".to_string()),
        );
        assert!(!case.result.is_pass());
        if let CaseResult::Fail { reason, details } = case.result {
            assert!(!reason.contains("ours"), "{reason}");
            assert!(reason.contains("upstream"), "{reason}");
            assert!(details.iter().any(|d| d.contains("forkpty")), "{details:?}");
        }
    }

    /// Two empty captures are not parity; a real TUI always draws something.
    #[test]
    fn two_empty_captures_fail() {
        let case = evaluate_capture("single-c".to_string(), capture(b""), capture(b""));
        assert!(!case.result.is_pass());
        if let CaseResult::Fail { reason, .. } = case.result {
            assert!(reason.contains("empty"), "{reason}");
        }
    }

    #[test]
    fn identical_captures_pass() {
        let screen = b"$ git ch".to_vec();
        let case = evaluate_capture("single-c".to_string(), Ok(screen.clone()), Ok(screen));
        assert!(case.result.is_pass(), "{:?}", case.result);
    }

    #[test]
    fn differing_captures_fail_with_cell_count() {
        let case = evaluate_capture(
            "single-c".to_string(),
            capture(b"$ hello"),
            capture(b"$ goodbye"),
        );
        assert!(!case.result.is_pass());
    }

    /// The child environment carries the same contract as
    /// `super::deterministic`: machine-specific inputs stripped,
    /// INSH_RS_NO_COREUTILS pinned.
    #[test]
    fn child_envp_strips_machine_specific_vars_and_pins_home() {
        let base: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
            ("PATH".into(), "/usr/bin".into()),
            ("INSH_RS_SPECS_DIR".into(), "/tmp/operator-specs".into()),
            ("ISTERM".into(), "1".into()),
            ("COLORTERM".into(), "truecolor".into()),
        ];
        let home = std::env::temp_dir();
        let text: Vec<String> = child_envp_from(base, &home)
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert!(
            !text.iter().any(|v| v.starts_with("INSH_RS_SPECS_DIR")),
            "{text:?}"
        );
        assert!(!text.iter().any(|v| v.starts_with("ISTERM=")), "{text:?}");
        assert!(!text.iter().any(|v| v == "COLORTERM=truecolor"), "{text:?}");
        assert!(
            text.iter()
                .any(|v| *v == format!("HOME={}", home.display())),
            "{text:?}"
        );
        assert!(text.iter().any(|v| v == "PATH=/usr/bin"), "{text:?}");
        assert!(text.iter().any(|v| v == "INSH_RS_NO_COREUTILS=1"));
        assert!(text.iter().any(|v| v == "TERM=xterm-256color"), "{text:?}");
    }
}
