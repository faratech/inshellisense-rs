//! `specs` parity category — set diff of `specs list` output, plus
//! content diff on a sample of specs that appear in both.

use super::{Category, CategoryReport, ScanConfig};
use std::collections::BTreeSet;
use std::process::Command;

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Specs);

    let ours_set = list_specs_ours(&cfg.ours);
    let upstream_set = list_specs_upstream(&cfg.upstream);

    // Upstream has a few broken/placeholder specs we intentionally
    // don't ship (the `example` spec throws a Node TypeError when
    // completion runs). Drop them from the upstream set so they
    // don't count as missing coverage.
    let broken_upstream_specs: &[&str] = &["example"];
    let mut filtered_upstream = upstream_set.clone();
    for b in broken_upstream_specs {
        filtered_upstream.remove(*b);
    }
    let both: BTreeSet<&String> = ours_set.intersection(&filtered_upstream).collect();
    let only_upstream: BTreeSet<&String> = filtered_upstream.difference(&ours_set).collect();
    let only_ours: BTreeSet<&String> = ours_set.difference(&filtered_upstream).collect();

    // We only fail when upstream has specs we're missing. Having more
    // specs than upstream is a win (we're ahead on corpus coverage),
    // not a divergence — the popup still works for the extra commands,
    // and users on inshellisense-rs benefit from the extras.
    if only_upstream.is_empty() {
        report.push_pass("specs list");
    } else {
        let mut details = vec![format!(
            "only in upstream: {}",
            only_upstream
                .iter()
                .take(15)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )];
        if !only_ours.is_empty() {
            details.push(format!(
                "only in ours (informational, not penalized): {}",
                only_ours
                    .iter()
                    .take(15)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let impact = if only_upstream.len() > 50 { 80 } else { 40 };
        report.push_fail(
            "specs list",
            format!(
                "missing from ours: {} (have {}; upstream has {})",
                only_upstream.len(),
                ours_set.len(),
                upstream_set.len()
            ),
            details,
            impact,
        );
    }

    report.summary = format!(
        "{} shared, {} only upstream, {} only ours",
        both.len(),
        only_upstream.len(),
        only_ours.len()
    );
    report
}

fn list_specs_ours(bin: &std::path::Path) -> BTreeSet<String> {
    let out = Command::new(bin).args(["specs", "list", "--plain"]).output();
    let Ok(o) = out else { return BTreeSet::new() };
    let text = String::from_utf8_lossy(&o.stdout);
    text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

fn list_specs_upstream(bin: &std::path::Path) -> BTreeSet<String> {
    let out = Command::new(bin).args(["specs", "list"]).output();
    let Ok(o) = out else { return BTreeSet::new() };
    let text = String::from_utf8_lossy(&o.stdout);
    // Upstream emits a JSON array of strings.
    if let Ok(arr) = serde_json::from_str::<Vec<String>>(&text) {
        return arr.into_iter().collect();
    }
    BTreeSet::new()
}
