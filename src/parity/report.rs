//! Markdown report emitter for the parity scanner.

use super::{CaseResult, Report};
use std::path::Path;
use std::time::SystemTime;

pub fn write_markdown(report: &Report, out_path: &Path) -> std::io::Result<()> {
    let mut s = String::new();
    s.push_str("# Parity scan — insh-rs vs upstream inshellisense\n\n");
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
    all_fails.sort_by(|a, b| b.1.impact.cmp(&a.1.impact));

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

    std::fs::write(out_path, s)
}
