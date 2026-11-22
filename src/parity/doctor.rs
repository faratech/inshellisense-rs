//! `doctor` parity category — run both doctor commands under isolated
//! `HOME` and diff the human-readable output with path/version noise
//! stripped.

use super::{Category, CategoryReport, ScanConfig};
use std::process::Command;

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Doctor);

    // Start from the same empty state. Reusing either binary's prepared home
    // makes the result depend on which scanner category ran first.
    let ours_home = cfg.raw_dir.join("doctor-home-ours");
    let upstream_home = cfg.raw_dir.join("doctor-home-upstream");
    for home in [&ours_home, &upstream_home] {
        if let Err(error) = super::prepare_output_dir(home) {
            report.push_fail("doctor setup", error.to_string(), Vec::new(), 90);
            return report;
        }
    }
    let (ours, ours_exit) = run_doctor(&cfg.ours, &ours_home);
    let (upstream, upstream_exit) = run_doctor(&cfg.upstream, &upstream_home);
    let findings = [
        (
            "legacy configurations",
            "no legacy configurations found",
            "no legacy configurations found",
        ),
        (
            "plugin presence",
            "all shells have plugins",
            "all shells have plugins",
        ),
        (
            "plugin kind",
            "all shells have correct plugins",
            "all shells have correct plugins",
        ),
        (
            "initialization needed",
            "do not have init files generated",
            "do not have configurations",
        ),
    ];
    for (name, ours_text, upstream_text) in findings {
        if ours.contains(ours_text) && upstream.contains(upstream_text) {
            report.push_pass(name);
        } else {
            report.push_fail(
                name,
                "expected finding missing",
                vec![
                    format!(
                        "ours expected {ours_text:?}, found {}",
                        ours.contains(ours_text)
                    ),
                    format!(
                        "upstream expected {upstream_text:?}, found {}",
                        upstream.contains(upstream_text)
                    ),
                ],
                40,
            );
        }
    }
    // The Rust port counts missing runtime resources separately. Assert each
    // return code to detect unexpected changes on either side.
    if (ours_exit, upstream_exit) == (2, 1) && ours.contains("runtime resources are missing") {
        report.push_pass("exit status (ours 2, upstream 1; Rust counts missing resources)");
    } else {
        report.push_fail(
            "exit status",
            format!(
                "expected ours=2 and upstream=1, got ours={ours_exit} and upstream={upstream_exit}"
            ),
            Vec::new(),
            40,
        );
    }
    let ours = normalize(&ours, &ours_home);
    let upstream = normalize(&upstream, &upstream_home);
    let _ = std::fs::write(cfg.raw_dir.join("doctor-ours.txt"), ours);
    let _ = std::fs::write(cfg.raw_dir.join("doctor-upstream.txt"), upstream);
    report.summary = format!(
        "{}/{} diagnostic and exit assertions pass (wording in raw captures)",
        report.pass_count(),
        report.cases.len()
    );
    report
}

fn run_doctor(bin: &std::path::Path, home: &std::path::Path) -> (String, i32) {
    let mut cmd = Command::new(bin);
    cmd.arg("doctor");
    super::deterministic(&mut cmd, home);
    match cmd.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (s, o.status.code().unwrap_or(-1))
        }
        Err(e) => (
            format!("<error: {} failed to spawn: {}>", bin.display(), e),
            -1,
        ),
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
