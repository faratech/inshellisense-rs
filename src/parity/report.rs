//! Markdown report emitter for the parity scanner.

use super::{CaseResult, Report};
use std::path::Path;
use std::time::SystemTime;

pub fn write_markdown(report: &Report, out_path: &Path) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("# Parity scan — inshellisense-rs vs upstream inshellisense\n\n");
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    s.push_str(&format!("Generated at unix={}\n\n", ts));

    // Summary table
    s.push_str("## Summary\n\n");
    s.push_str("| Category | Score | PASS | FAIL | Details |\n");
    s.push_str("|----------|------:|-----:|-----:|---------|\n");
    for cat in &report.categories {
        s.push_str(&format!(
            "| {} | {:.1}% | {} | {} | {} |\n",
            cat.category.name(),
            cat.score() * 100.0,
            cat.pass_count(),
            cat.fail_count(),
            cat.summary
        ));
    }
    s.push('\n');

    // Top 10 divergences
    let mut all_fails: Vec<(&super::CategoryReport, &super::Case)> = Vec::new();
    for cat in &report.categories {
        for case in &cat.cases {
            if !case.result.is_pass() {
                all_fails.push((cat, case));
            }
        }
    }
    all_fails.sort_by_key(|b| std::cmp::Reverse(b.1.impact));

    s.push_str("## Top divergences (ranked by impact)\n\n");
    if all_fails.is_empty() {
        s.push_str("_No failures recorded._ 🎉\n\n");
    } else {
        for (idx, (cat, case)) in all_fails.iter().take(10).enumerate() {
            s.push_str(&format!(
                "### {}. [{}/{}] (impact={})\n\n",
                idx + 1,
                cat.category.name(),
                case.name,
                case.impact
            ));
            if let CaseResult::Fail { reason, details } = &case.result {
                s.push_str(&format!("**{}**\n\n", reason));
                if !details.is_empty() {
                    s.push_str("```\n");
                    for d in details {
                        s.push_str(d);
                        s.push('\n');
                    }
                    s.push_str("```\n\n");
                }
            }
        }
    }

    // Per-category drilldown
    s.push_str("## Per-category drilldown\n\n");
    for cat in &report.categories {
        s.push_str(&format!(
            "### {} ({}/{} pass)\n\n",
            cat.category.name(),
            cat.pass_count(),
            cat.cases.len()
        ));
        for case in &cat.cases {
            let status = if case.result.is_pass() { "✓" } else { "✗" };
            s.push_str(&format!("- {} `{}`", status, case.name));
            if let CaseResult::Fail { reason, .. } = &case.result {
                s.push_str(&format!(" — {}", reason));
            }
            s.push('\n');
        }
        s.push('\n');
    }

    write_report_file(out_path, s.as_bytes())
}

/// Write the report, creating its directory. On Unix a symlink at the report
/// path is refused rather than followed, so a link planted at a predictable
/// path cannot redirect the write onto another file the developer can write.
fn write_report_file(out_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = out_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(out_path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report is written in place of a regular file and never through a
    /// symlink planted at its path (#88).
    #[cfg(unix)]
    #[test]
    fn report_write_refuses_a_symlink_and_creates_its_directory() {
        let dir = crate::test_support::unique_temp_dir("report");
        let path = dir.join("nested").join("report.md");
        write_report_file(&path, b"first").unwrap();
        write_report_file(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let link = dir.join("link.md");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(write_report_file(&link, b"clobbered").is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(dir);
    }
}
