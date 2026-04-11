//! `render` parity category — drive both binaries' interactive popup
//! through a fixed PTY, capture output, replay through vt100, and diff
//! the resulting screen grids cell-by-cell.
//!
//! This is the most important category because rendering is the thing
//! the user visually compares.

use super::{Case, CaseResult, Category, CategoryReport, ScanConfig};
use portable_pty::{CommandBuilder, PtySize};
use rayon::prelude::*;
use serde::Deserialize;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
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
    let cases: Vec<Case> = scenarios
        .par_iter()
        .map(|scenario| {
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
        })
        .collect();
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
    let pty_system = portable_pty::native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: PTY_ROWS,
        cols: PTY_COLS,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut cmd = CommandBuilder::new(bin);
    cmd.arg("start");
    // Force pure popup mode so our Hybrid (ghost+popup) doesn't
    // draw ghost text that upstream doesn't emit. Upstream doesn't
    // understand --ui but tolerates unknown args gracefully on its
    // own `start` subcommand (flag is a no-op there).
    cmd.args(["--ui", "popup"]);
    cmd.env("TERM", "xterm-256color");
    // Intentionally don't set COLORTERM so both binaries fall back to
    // the 256-color path (chalk's default on Linux ttys).
    cmd.env_remove("COLORTERM");
    // Both binaries need to find their resource trees (upstream
    // wants `~/.inshellisense/`, ours wants `~/.insh-rs/`). Using an
    // isolated HOME means neither exists → upstream prints
    // "resources out of date". Keep the real HOME so both see their
    // installed resources.
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env("PS1", "$ ");
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return Vec::new(),
    };

    // Reader thread → shared buffer.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut captured = Vec::new();
    let drain_into = |captured: &mut Vec<u8>, rx: &std::sync::mpsc::Receiver<Vec<u8>>, dur: Duration| {
        let end = Instant::now() + dur;
        while Instant::now() < end {
            if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
                captured.extend_from_slice(&chunk);
            }
        }
    };

    drain_into(&mut captured, &rx, Duration::from_millis(5000));

    // Type the scenario keys one char at a time with a generous
    // per-char settle. Upstream's `getSuggestions()` is fully async
    // and takes ~500-1000ms per invocation (Node SEA startup + spec
    // lookup). If we type too fast, upstream falls behind and
    // emits clear-only cycles for the trailing chars, leaving the
    // popup showing results for the first keystroke only. 800ms
    // matches upstream's typical resolve latency on this machine.
    for ch in scenario.keys.chars() {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        let _ = writer.write_all(s.as_bytes());
        let _ = writer.flush();
        drain_into(&mut captured, &rx, Duration::from_millis(800));
    }

    // Wait for async suggestion resolution.
    drain_into(&mut captured, &rx, Duration::from_millis(scenario.settle_ms));

    // Optional follow-up keys (e.g. tab, escape).
    if let Some(then) = &scenario.then {
        for ch in then.chars() {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            let _ = writer.write_all(s.as_bytes());
            let _ = writer.flush();
            drain_into(&mut captured, &rx, Duration::from_millis(150));
        }
        drain_into(
            &mut captured,
            &rx,
            Duration::from_millis(scenario.then_settle_ms.unwrap_or(500)),
        );
    }

    // Ctrl-C + exit to shut down cleanly.
    let _ = writer.write_all(b"\x03");
    let _ = writer.flush();
    drain_into(&mut captured, &rx, Duration::from_millis(200));
    let _ = writer.write_all(b"exit\r");
    let _ = writer.flush();
    drain_into(&mut captured, &rx, Duration::from_millis(500));

    let _ = child.kill();
    let _ = child.wait();

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
