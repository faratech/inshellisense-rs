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

    // Same scratch HOME for both sides, so the operator's spec sources
    // (INSH_RS_SPECS_DIR, rc.toml `[specs].path`, user TOML specs) cannot
    // configure our side and not upstream's.
    let home = super::isolated_home(cfg);

    let (ours_set, upstream_set) = match (
        list_specs_ours(&cfg.ours, &home),
        list_specs_upstream(&cfg.upstream, &home),
    ) {
        (Ok(o), Ok(u)) => (o, u),
        (ours, upstream) => {
            // A failed or unparseable listing is a scanner failure. It
            // used to collapse to an empty set, so two broken children
            // compared as an identical (empty) spec list and the
            // category scored a vacuous 100%.
            let mut reasons = Vec::new();
            for (side, result) in [("ours", ours), ("upstream", upstream)] {
                if let Err(e) = result {
                    reasons.push(format!("{side}: {e}"));
                }
            }
            report.push_fail(
                "specs list",
                format!("could not list specs — {}", reasons.join("; ")),
                Vec::new(),
                3,
            );
            return report;
        }
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
            top_level_suggestions(&cfg.ours, name, true, &home),
            top_level_suggestions(&cfg.upstream, name, false, &home),
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
    home: &std::path::Path,
) -> Option<BTreeSet<String>> {
    let mut cmd = Command::new(bin);
    super::deterministic(&mut cmd, home);
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

/// `Err` means the listing cannot be trusted: the binary would not run,
/// exited non-zero, or reported nothing. Collapsing any of those to an
/// empty set made two failed children compare as an identical (empty)
/// spec list — perfect parity while comparing nothing. Neither binary can
/// legitimately report zero specs, so an empty listing is a failure too.
fn list_specs_ours(
    bin: &std::path::Path,
    home: &std::path::Path,
) -> Result<BTreeSet<String>, String> {
    let mut cmd = Command::new(bin);
    super::deterministic(&mut cmd, home);
    let out = cmd
        .args(["specs", "list", "--plain"])
        .output()
        .map_err(|e| format!("`specs list --plain` failed to spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`specs list --plain` exited with {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let specs: BTreeSet<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if specs.is_empty() {
        return Err("`specs list --plain` printed nothing".to_string());
    }
    Ok(specs)
}

/// Upstream's spec surface, or `Err` when its listing cannot be trusted.
fn list_specs_upstream(
    bin: &std::path::Path,
    home: &std::path::Path,
) -> Result<BTreeSet<String>, String> {
    let mut cmd = Command::new(bin);
    super::deterministic(&mut cmd, home);
    let out = cmd
        .args(["specs", "list"])
        .output()
        .map_err(|e| format!("`specs list` failed to spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`specs list` exited with {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Upstream emits a JSON array of strings. Anything else (empty stdout,
    // prose on stdout, a truncated payload) is a failure, not an empty
    // spec list.
    let arr: Vec<String> = serde_json::from_str(&text)
        .map_err(|e| format!("`specs list` printed unparseable output ({e}): {text:.200}"))?;
    if arr.is_empty() {
        return Err("`specs list` printed an empty spec list".to_string());
    }
    Ok(arr.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch HOME handed to [`super::deterministic`] for stub children.
    #[cfg(unix)]
    fn scratch_home() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("insh-parity-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stub binary whose `specs list` runs `script_body`, so each way a
    /// listing can be untrustworthy is reproducible without the real tool.
    #[cfg(unix)]
    fn stub_specs_list(script_body: &str) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "insh-parity-specs-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("stub-is");
        let mut f = std::fs::File::create(&script).unwrap();
        write!(f, "#!/bin/sh\n{script_body}").unwrap();
        drop(f);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A freshly written script occasionally refuses to exec with
        // ETXTBSY on container filesystems; wait until the kernel accepts
        // it so the tests exercise the listing logic, not the filesystem.
        for _ in 0..200 {
            match Command::new(&script).arg("probe").output() {
                Ok(_) => break,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        script
    }

    /// This exact shape used to score perfect parity: a failing child
    /// collapsed to an empty set that matched another failure's empty set.
    #[cfg(unix)]
    #[test]
    fn upstream_nonzero_exit_is_a_failure_not_an_empty_set() {
        let bin = stub_specs_list("echo error >&2\nexit 1\n");
        let err = list_specs_upstream(&bin, &scratch_home()).unwrap_err();
        assert!(err.contains("exited with"), "err={err}");
    }

    #[cfg(unix)]
    #[test]
    fn upstream_prose_stdout_is_a_failure_not_an_empty_set() {
        let bin = stub_specs_list("echo 'unknown command'\nexit 0\n");
        let err = list_specs_upstream(&bin, &scratch_home()).unwrap_err();
        assert!(err.contains("unparseable"), "err={err}");
    }

    #[cfg(unix)]
    #[test]
    fn upstream_empty_array_is_a_failure() {
        // Neither binary can legitimately report zero specs.
        let bin = stub_specs_list("echo '[]'\nexit 0\n");
        let err = list_specs_upstream(&bin, &scratch_home()).unwrap_err();
        assert!(err.contains("empty spec list"), "err={err}");
    }

    #[cfg(unix)]
    #[test]
    fn upstream_valid_array_parses_to_a_set() {
        let bin = stub_specs_list(r#"echo '["git","ls"]'"#);
        let specs = list_specs_upstream(&bin, &scratch_home()).unwrap();
        assert_eq!(specs, BTreeSet::from(["git".to_string(), "ls".to_string()]));
    }

    #[cfg(unix)]
    #[test]
    fn ours_empty_listing_is_a_failure() {
        let bin = stub_specs_list("exit 0\n");
        assert!(list_specs_ours(&bin, &scratch_home()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ours_plain_lines_parse_to_a_set() {
        let bin = stub_specs_list(r#"printf 'git\n\n  ls  \n'"#);
        let specs = list_specs_ours(&bin, &scratch_home()).unwrap();
        assert_eq!(specs, BTreeSet::from(["git".to_string(), "ls".to_string()]));
    }
}
