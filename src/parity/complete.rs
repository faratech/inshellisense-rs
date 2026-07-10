//! `complete` parity category — the most valuable category.
//!
//! For each `{"line": "…", "cwd": "…"}` entry in `complete.jsonl`,
//! run `{ours} complete <line> --json` and `{upstream} complete <line>`,
//! parse both into a common `NormalizedBlob`, and diff field by field.
//!
//! Both binaries emit JSON. The shapes differ: upstream wraps the
//! suggestions in `{"suggestions": [...], "activeToken": {...}}`, ours
//! emits a flat `[...]`. The normalizer handles both.

use super::{Case, CaseResult, Category, CategoryReport, ScanConfig};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedBlob {
    pub suggestions: Vec<NormalizedSuggestion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedSuggestion {
    pub name: String,
    pub ty: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CorpusEntry {
    line: String,
    #[serde(default)]
    cwd: Option<String>,
    /// Optional human label for the report.
    #[serde(default)]
    label: Option<String>,
}

pub fn run(cfg: &ScanConfig) -> CategoryReport {
    let mut report = CategoryReport::new(Category::Complete);

    let fixture_cwd = cfg
        .corpus_dir
        .join("fixtures")
        .join("cwd")
        .canonicalize()
        .unwrap_or_else(|_| cfg.corpus_dir.join("fixtures").join("cwd"));
    let corpus_path = cfg.corpus_dir.join("complete.jsonl");
    // A missing, empty, or all-malformed corpus proves nothing. Reporting it
    // as "skipped" left the category with zero cases, which used to score
    // 100% and pass the threshold — silently disabling coverage.
    let entries = match load_corpus(&corpus_path) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => {
            report.push_fail(
                "corpus",
                format!("corpus at {} has no usable cases", corpus_path.display()),
                Vec::new(),
                3,
            );
            return report;
        }
        Err(e) => {
            report.push_fail(
                "corpus",
                format!("cannot read corpus at {}: {e}", corpus_path.display()),
                Vec::new(),
                3,
            );
            return report;
        }
    };

    // Run all test cases in parallel via std::thread::scope.
    let cases: Vec<Case> = std::thread::scope(|s| {
        let handles: Vec<_> = entries
            .iter()
            .map(|entry| {
                s.spawn(|| {
                    let label = entry.label.clone().unwrap_or_else(|| entry.line.clone());

                    let ours_json = run_complete(
                        &cfg.ours,
                        &entry.line,
                        entry.cwd.as_deref(),
                        true,
                        &fixture_cwd,
                    );
                    let upstream_json = run_complete(
                        &cfg.upstream,
                        &entry.line,
                        entry.cwd.as_deref(),
                        false,
                        &fixture_cwd,
                    );

                    let ours_blob = match parse_ours(&ours_json) {
                        Ok(b) => b,
                        Err(e) => {
                            return Case {
                                name: label,
                                result: CaseResult::Fail {
                                    reason: format!("failed to parse ours: {}", e),
                                    details: vec![ours_json.chars().take(200).collect()],
                                },
                                impact: 90,
                            };
                        }
                    };
                    let upstream_blob = match parse_upstream(&upstream_json) {
                        Ok(b) => b,
                        Err(e) => {
                            return Case {
                                name: label,
                                result: CaseResult::Fail {
                                    reason: format!("failed to parse upstream: {}", e),
                                    details: vec![upstream_json.chars().take(200).collect()],
                                },
                                impact: 90,
                            };
                        }
                    };

                    let diff = diff_blobs(&upstream_blob, &ours_blob);
                    if diff.is_empty() {
                        Case {
                            name: label,
                            result: CaseResult::Pass,
                            impact: 0,
                        }
                    } else {
                        let impact = classify_impact(&upstream_blob, &ours_blob);
                        let reason = summarize(&diff);
                        Case {
                            name: label,
                            result: CaseResult::Fail {
                                reason,
                                details: diff,
                            },
                            impact,
                        }
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    report.cases = cases;

    report.summary = format!(
        "{}/{} completion cases match",
        report.pass_count(),
        report.cases.len()
    );
    report
}

/// A malformed JSONL line is a corpus regression, not a line to skip.
fn load_corpus(path: &Path) -> std::io::Result<Vec<CorpusEntry>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<CorpusEntry>(line) {
            Ok(entry) => out.push(entry),
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

fn run_complete(
    bin: &Path,
    line: &str,
    cwd: Option<&str>,
    ours: bool,
    fixture_cwd: &Path,
) -> String {
    let mut cmd = Command::new(bin);
    super::deterministic(&mut cmd);
    cmd.arg("complete");
    if ours {
        cmd.arg("--json");
    }
    cmd.arg(line);
    // Both binaries need to run in the SAME cwd or their filepath
    // templates return different entries.
    let resolved_cwd = cwd
        .filter(|s| !s.is_empty() && *s != ".")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| fixture_cwd.to_path_buf());
    cmd.current_dir(&resolved_cwd);
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => format!("<error: {}>", e),
    }
}

/// Parse our flat array output.
fn parse_ours(s: &str) -> Result<NormalizedBlob, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(NormalizedBlob {
            suggestions: Vec::new(),
        });
    }
    #[derive(Deserialize)]
    struct RowOurs {
        name: String,
        #[serde(rename = "type", default)]
        ty: String,
        #[serde(default)]
        description: Option<String>,
    }
    let rows: Vec<RowOurs> = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    Ok(NormalizedBlob {
        suggestions: rows
            .into_iter()
            .map(|r| NormalizedSuggestion {
                name: r.name,
                ty: normalize_type(&r.ty),
                description: r.description,
            })
            .collect(),
    })
}

/// Parse upstream's `{suggestions, activeToken}` wrapper.
fn parse_upstream(s: &str) -> Result<NormalizedBlob, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(NormalizedBlob {
            suggestions: Vec::new(),
        });
    }
    // Upstream sometimes prints nothing when there's no active suggestion.
    #[derive(Deserialize)]
    struct RowUpstream {
        name: String,
        #[serde(default, rename = "type")]
        ty: Option<String>,
        #[serde(default)]
        description: Option<String>,
    }
    #[derive(Deserialize)]
    struct UpstreamWrapper {
        #[serde(default)]
        suggestions: Vec<RowUpstream>,
    }
    // Try wrapper first, then fall back to bare array.
    if let Ok(w) = serde_json::from_str::<UpstreamWrapper>(trimmed) {
        return Ok(NormalizedBlob {
            suggestions: w
                .suggestions
                .into_iter()
                .map(|r| NormalizedSuggestion {
                    name: r.name,
                    ty: normalize_type(r.ty.as_deref().unwrap_or("")),
                    description: r.description,
                })
                .collect(),
        });
    }
    let rows: Vec<RowUpstream> = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    Ok(NormalizedBlob {
        suggestions: rows
            .into_iter()
            .map(|r| NormalizedSuggestion {
                name: r.name,
                ty: normalize_type(r.ty.as_deref().unwrap_or("")),
                description: r.description,
            })
            .collect(),
    })
}

/// Strip trailing `/` from folder names so our `sub1/` compares equal
/// to upstream's `sub1`. Upstream emits bare directory names in the
/// `complete` JSON — the `/` suffix is a ghost-text hint only, not a
/// field we should compare.
fn normalize_name(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

fn normalize_type(s: &str) -> String {
    let lower = s.to_lowercase();
    match lower.as_str() {
        "subcommand" | "command" => "subcommand".to_string(),
        "option" | "flag" => "option".to_string(),
        "folder" | "directory" | "dir" => "folder".to_string(),
        "file" => "file".to_string(),
        "arg" | "argument" => "arg".to_string(),
        "special" => "special".to_string(),
        "mixin" => "mixin".to_string(),
        "shortcut" => "shortcut".to_string(),
        _ => lower,
    }
}

fn diff_blobs(upstream: &NormalizedBlob, ours: &NormalizedBlob) -> Vec<String> {
    let mut out = Vec::new();
    let up_count = upstream.suggestions.len();
    let our_count = ours.suggestions.len();
    let cmp_len = up_count.min(our_count).min(5);

    // Top-N name sequence. We deliberately do NOT diff the `type`
    // field: upstream's JSON omits it entirely, so we'd flag every
    // single suggestion as "type mismatch" — which is noise, not
    // signal.
    for i in 0..cmp_len {
        let u = normalize_name(&upstream.suggestions[i].name);
        let o = normalize_name(&ours.suggestions[i].name);
        if u != o {
            out.push(format!("top[{}]: upstream={} ours={}", i, u, o));
        }
    }

    // Only flag count differences when ours is *missing* suggestions
    // upstream returned. Having MORE suggestions than upstream is a
    // corpus-drift win, not a divergence — it means our spec corpus
    // is broader and the top-N we compared is still correct.
    if our_count + 5 < up_count {
        out.push(format!(
            "count: upstream={} ours={} (missing suggestions)",
            up_count, our_count
        ));
    }

    // Set diff — only report upstream's names we're missing. Names
    // we have but upstream doesn't are informational.
    use std::collections::BTreeSet;
    let up_set: BTreeSet<String> = upstream
        .suggestions
        .iter()
        .map(|s| normalize_name(&s.name))
        .collect();
    let our_set: BTreeSet<String> = ours
        .suggestions
        .iter()
        .map(|s| normalize_name(&s.name))
        .collect();
    let missing: Vec<String> = up_set.difference(&our_set).cloned().collect();
    if !missing.is_empty() {
        out.push(format!(
            "missing from ours: {}",
            missing
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    out
}

fn summarize(diff: &[String]) -> String {
    if diff.is_empty() {
        return String::new();
    }
    diff[0].clone()
}

fn classify_impact(up: &NormalizedBlob, ours: &NormalizedBlob) -> u8 {
    // Top-of-list mismatches are high impact; count-only mismatches
    // (same top N) are lower.
    match (up.suggestions.first(), ours.suggestions.first()) {
        (Some(u), Some(o)) if u.name != o.name => 90,
        (Some(_), Some(_)) if up.suggestions.len() != ours.suggestions.len() => 50,
        (Some(_), None) | (None, Some(_)) => 80,
        _ => 30,
    }
}
