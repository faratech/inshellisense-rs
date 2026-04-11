//! `init` parity category — diff `init <shell>` snippets.

use super::{Category, CategoryReport, ScanConfig};
use std::process::Command;

const SHELLS: &[&str] = &["bash", "zsh", "fish", "pwsh", "xonsh", "nu"];

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Init);
    for shell in SHELLS {
        let ours = run_init(&cfg.ours, shell);
        let upstream = run_init(&cfg.upstream, shell);
        let ours_n = normalize(&ours);
        let up_n = normalize(&upstream);
        if ours_n == up_n {
            report.push_pass(*shell);
            continue;
        }
        let details = line_diff(&up_n, &ours_n);
        report.push_fail(
            *shell,
            format!("init snippet diverges ({} differing lines)", details.len()),
            details.into_iter().take(20).collect(),
            50,
        );
    }
    report.summary = format!(
        "{}/{} shell init snippets match",
        report.pass_count(),
        report.cases.len()
    );
    report
}

fn run_init(bin: &std::path::Path, shell: &str) -> String {
    let out = Command::new(bin).args(["init", shell]).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => format!("<error: {}>", e),
    }
}

/// Strip paths and names that are allowed to differ so we compare the
/// structural shape of the snippet rather than implementation paths.
fn normalize(s: &str) -> String {
    let mut out = s.to_string();
    // Path replacements — order matters: longest first.
    let paths = [
        "/root/.insh-rs",
        "/root/.inshellisense",
        "~/.insh-rs",
        "~/.inshellisense",
    ];
    for p in paths {
        out = out.replace(p, "<CACHE>");
    }
    // Program names
    for p in ["inshellisense", "insh-rs", "insh", "ISTERM", "INSH_RS"] {
        out = out.replace(p, "<PROG>");
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
