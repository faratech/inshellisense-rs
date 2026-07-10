//! `specs` parity category — set diff of `specs list` output, plus a content
//! diff on a sample of specs that appear in both: for each sampled name we
//! compare the top-level suggestions both binaries offer for `<name> `, so a
//! shared spec that lost its subcommands or options fails the category rather
//! than passing on its name alone.

use super::{Category, CategoryReport, ScanConfig};
use std::collections::BTreeSet;
use std::process::Command;

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Specs);

    let (Some(ours_set), Some(upstream_set)) = (
        list_specs_ours(&cfg.ours),
        list_specs_upstream(&cfg.upstream),
    ) else {
        report.push_fail(
            "specs list",
            "could not run `specs list` on both binaries".to_string(),
            Vec::new(),
            3,
        );
        return report;
    };

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

    // Content diff over a deterministic sample of shared specs. Without this
    // the category only ever compared *names*, so a shared spec that lost all
    // of its subcommands still scored perfect parity.
    let sample: Vec<&String> = both
        .iter()
        .copied()
        .step_by(sample_stride(both.len()))
        .collect();
    let mut compared = 0;
    for name in &sample {
        let (Some(ours), Some(upstream)) = (
            top_level_suggestions(&cfg.ours, name, true),
            top_level_suggestions(&cfg.upstream, name, false),
        ) else {
            report.push_fail(
                format!("specs content: {name}"),
                "could not run `complete` on both binaries".to_string(),
                Vec::new(),
                3,
            );
            continue;
        };
        compared += 1;
        // Upstream is the reference: everything it offers, we must offer.
        let missing: Vec<&String> = upstream.difference(&ours).collect();
        if missing.is_empty() {
            report.push_pass(format!("specs content: {name}"));
        } else {
            report.push_fail(
                format!("specs content: {name}"),
                format!("{} suggestion(s) missing from our spec", missing.len()),
                vec![format!(
                    "missing: {}",
                    missing
                        .iter()
                        .take(15)
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )],
                40,
            );
        }
    }

    report.summary = format!(
        "{} shared, {} only upstream, {} only ours, {} content-compared",
        both.len(),
        only_upstream.len(),
        only_ours.len(),
        compared
    );
    report
}

/// Sample at most `MAX_CONTENT_SAMPLE` shared specs, spread across the sorted
/// set so the sample isn't biased toward names starting with `a`.
fn sample_stride(total: usize) -> usize {
    const MAX_CONTENT_SAMPLE: usize = 25;
    (total / MAX_CONTENT_SAMPLE).max(1)
}

/// The set of suggestion names each binary offers for `<spec> ` — i.e. the
/// spec's top-level subcommands and options. `None` if the binary can't run.
fn top_level_suggestions(
    bin: &std::path::Path,
    spec: &str,
    ours: bool,
) -> Option<BTreeSet<String>> {
    let mut cmd = Command::new(bin);
    cmd.arg("complete");
    if ours {
        cmd.arg("--json");
    }
    let out = cmd.arg(format!("{spec} ")).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    // Ours: {"suggestions":[{"name":...}]}. Upstream: same shape.
    let rows = json
        .get("suggestions")
        .and_then(|s| s.as_array())
        .or_else(|| json.as_array())?;
    Some(
        rows.iter()
            .filter_map(|row| match row.get("name") {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                // `name` may be an alias array; the first entry is primary.
                Some(serde_json::Value::Array(a)) => {
                    a.first().and_then(|v| v.as_str()).map(str::to_string)
                }
                _ => None,
            })
            .collect(),
    )
}

/// `None` means the binary could not be run. Collapsing that to an empty set
/// made two failed spawns compare as an identical (empty) spec list.
fn list_specs_ours(bin: &std::path::Path) -> Option<BTreeSet<String>> {
    let out = Command::new(bin)
        .args(["specs", "list", "--plain"])
        .output();
    let Ok(o) = out else { return None };
    let text = String::from_utf8_lossy(&o.stdout);
    Some(
        text.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

fn list_specs_upstream(bin: &std::path::Path) -> Option<BTreeSet<String>> {
    let out = Command::new(bin).args(["specs", "list"]).output();
    let Ok(o) = out else { return None };
    let text = String::from_utf8_lossy(&o.stdout);
    // Upstream emits a JSON array of strings.
    if let Ok(arr) = serde_json::from_str::<Vec<String>>(&text) {
        return Some(arr.into_iter().collect());
    }
    Some(BTreeSet::new())
}
