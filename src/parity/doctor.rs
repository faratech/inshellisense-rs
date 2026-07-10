//! `doctor` parity category — run both doctor commands under isolated
//! `HOME` and diff the human-readable output with path/version noise
//! stripped.

use super::{Category, CategoryReport, ScanConfig};
use std::process::Command;

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Doctor);

    let home = super::isolated_home(cfg);
    let ours = run_doctor(&cfg.ours, &home);
    let upstream = run_doctor(&cfg.upstream, &home);

    let ours_n = normalize(&ours, &home);
    let up_n = normalize(&upstream, &home);

    if ours_n == up_n {
        report.push_pass("doctor");
    } else {
        let details = line_diff(&up_n, &ours_n);
        report.push_fail(
            "doctor",
            format!("doctor output diverges ({} differing lines)", details.len()),
            details.into_iter().take(20).collect(),
            40,
        );
    }

    report.summary = format!(
        "{}/{} doctor checks match",
        report.pass_count(),
        report.cases.len()
    );
    report
}

fn run_doctor(bin: &std::path::Path, home: &std::path::Path) -> String {
    let mut cmd = Command::new(bin);
    cmd.arg("doctor");
    super::isolate(&mut cmd, home);
    match cmd.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        }
        Err(e) => format!("<error: {} failed to spawn: {}>", bin.display(), e),
    }
}

fn normalize(s: &str, home: &std::path::Path) -> String {
    let no_ansi = strip_ansi(s);
    let mut out = no_ansi;
    // Paths. Derive the cache path from the isolated HOME instead of
    // hardcoding `/root`, which only ever matched one developer's machine
    // (and was listed twice).
    let cache = home.join(".inshellisense");
    out = out.replace(&cache.display().to_string(), "<CACHE>");
    out = out.replace(&home.display().to_string(), "<HOME>");
    for p in ["inshellisense-rs", "inshellisense"] {
        out = out.replace(p, "<PROG>");
    }
    // Version strings: v1.2.3 or 1.2.3
    out = strip_versions(&out);
    out.lines()
        .map(|l| l.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_versions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() {
            // Walk forward as long as chars are digits or '.'.
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let run = &s[start..i];
            if run.contains('.') {
                out.push_str("<VER>");
            } else {
                out.push_str(run);
            }
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let c = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn line_diff(a: &str, b: &str) -> Vec<String> {
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let mut d = Vec::new();
    for i in 0..al.len().max(bl.len()) {
        let x = al.get(i).copied().unwrap_or("");
        let y = bl.get(i).copied().unwrap_or("");
        if x != y {
            if !x.is_empty() {
                d.push(format!("- {}", x));
            }
            if !y.is_empty() {
                d.push(format!("+ {}", y));
            }
        }
    }
    d
}
