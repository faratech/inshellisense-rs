//! `complete` parity category — the most valuable category.
//!
//! For each `{"line": "…", "cwd": "…"}` entry in `complete.jsonl`,
//! run `{ours} complete <line> --json` and `{upstream} complete <line>`,
//! parse both into a common `NormalizedBlob`, and diff field by field.
//!
//! Both binaries emit JSON with the same shape: suggestions wrapped in
//! `{"suggestions": [...], "activeToken": {...}}` (`src/commands/complete.rs`
//! builds that object unconditionally). A bare `[...]` array is accepted too,
//! so older builds still compare. Anything else fails the case.

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

    let ours_home = super::isolated_home_ours(cfg);
    let upstream_home = super::isolated_home_upstream(cfg);

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
                        &ours_home,
                    );
                    let upstream_json = run_complete(
                        &cfg.upstream,
                        &entry.line,
                        entry.cwd.as_deref(),
                        false,
                        &fixture_cwd,
                        &upstream_home,
                    );

                    let ours_blob = match parse_suggestions(&ours_json) {
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
                    let upstream_blob = match parse_suggestions(&upstream_json) {
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
    home: &Path,
) -> String {
    let mut cmd = Command::new(bin);
    super::deterministic(&mut cmd, home);
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
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            // A crashed child used to look identical to "no suggestions",
            // so two crashed binaries compared as equal and passed. Surface
            // the failure instead; parse errors carry this text into the
            // report.
            if stdout.trim().is_empty() && !o.status.success() {
                let stderr: String = String::from_utf8_lossy(&o.stderr)
                    .chars()
                    .take(200)
                    .collect();
                return format!(
                    "<error: exited with {}: {stderr}>",
                    o.status.code().unwrap_or(-1)
                );
            }
            stdout
        }
        Err(e) => format!("<error: {e}>"),
    }
}

/// Parse `complete` JSON from either binary.
///
/// Both sides emit the same `{"suggestions": [...], "activeToken": {...}}`
/// wrapper (`src/commands/complete.rs` builds it unconditionally, and so does
/// upstream); a bare `[...]` array is accepted so older builds still compare.
/// Anything else is an error: mapping unparseable output to "zero
/// suggestions" let two broken binaries compare as identical empty results.
fn parse_suggestions(s: &str) -> Result<NormalizedBlob, String> {
    let trimmed = s.trim();
    // A side that legitimately has nothing to offer prints nothing.
    // Upstream complete.ts has a known bug where `process.stdout.write(JSON.stringify(undefined))`
    // throws ERR_INVALID_ARG_TYPE at node:internal/streams/writable:482 when getSuggestions returns undefined (0 suggestions).
    if trimmed.is_empty()
        || trimmed == "null"
        || trimmed.contains("node:internal/streams/writable:482")
        || (trimmed.contains("ERR_INVALID_ARG_TYPE") && trimmed.contains("Received undefined"))
    {
        return Ok(NormalizedBlob {
            suggestions: Vec::new(),
        });
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    let rows = match value {
        serde_json::Value::Array(rows) => rows,
        serde_json::Value::Object(map) => match map.get("suggestions") {
            Some(serde_json::Value::Array(rows)) => rows.clone(),
            Some(other) => {
                return Err(format!(
                    "`suggestions` is {}, expected an array",
                    json_kind(other)
                ));
            }
            None => return Err("object has no `suggestions` array".to_string()),
        },
        other => {
            return Err(format!(
                "expected an object or array, got {}",
                json_kind(&other)
            ));
        }
    };
    let mut suggestions = Vec::with_capacity(rows.len());
    for row in rows {
        let name = match row.get("name") {
            Some(serde_json::Value::String(s)) => s.clone(),
            // `name` may be an alias array; the first entry is primary.
            Some(serde_json::Value::Array(a)) => a
                .first()
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| "`name` array is empty".to_string())?,
            _ => return Err(format!("suggestion row has no string `name`: {row}")),
        };
        let ty = row.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let description = row
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        suggestions.push(NormalizedSuggestion {
            name,
            ty: normalize_type(ty),
            description,
        });
    }
    Ok(NormalizedBlob { suggestions })
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape our binary actually emits (see `src/commands/complete.rs`):
    /// a wrapper object, not a flat array.
    #[test]
    fn parses_our_wrapper_object() {
        let json = r#"{"suggestions":[
            {"name":"checkout","type":"subcommand","description":"Switch branches"},
            {"name":"--all"}
        ],"activeToken":{"token":"ch"}}"#;
        let blob = parse_suggestions(json).unwrap();
        assert_eq!(blob.suggestions.len(), 2);
        assert_eq!(blob.suggestions[0].name, "checkout");
        assert_eq!(blob.suggestions[0].ty, "subcommand");
        assert_eq!(
            blob.suggestions[0].description.as_deref(),
            Some("Switch branches")
        );
        assert_eq!(blob.suggestions[1].name, "--all");
        assert_eq!(blob.suggestions[1].ty, "");
        assert_eq!(blob.suggestions[1].description, None);
    }

    /// Upstream's wrapper omits `type`; it must normalize to "" rather than
    /// failing the row.
    #[test]
    fn parses_upstream_wrapper_without_type() {
        let blob =
            parse_suggestions(r#"{"suggestions":[{"name":"cherry-pick"},{"name":"clean"}]}"#)
                .unwrap();
        assert_eq!(blob.suggestions.len(), 2);
        assert_eq!(blob.suggestions[0].name, "cherry-pick");
        assert_eq!(blob.suggestions[0].ty, "");
    }

    /// Older builds printed a bare array; keep comparing those.
    #[test]
    fn still_parses_a_bare_array() {
        let blob = parse_suggestions(r#"[{"name":"commit","type":"subcommand"}]"#).unwrap();
        assert_eq!(blob.suggestions.len(), 1);
        assert_eq!(blob.suggestions[0].ty, "subcommand");
    }

    /// An alias-style `name` array takes its first entry.
    #[test]
    fn takes_first_alias_as_name() {
        let blob = parse_suggestions(r#"{"suggestions":[{"name":["co","com"]}]}"#).unwrap();
        assert_eq!(blob.suggestions[0].name, "co");
    }

    /// Unparseable output must fail loudly: silently yielding "no
    /// suggestions" made two broken binaries compare as equal.
    #[test]
    fn rejects_an_object_without_suggestions() {
        let err = parse_suggestions(r#"{"error":"boom"}"#).unwrap_err();
        assert!(err.contains("suggestions"), "err={err}");
    }

    #[test]
    fn rejects_a_non_array_suggestions_field() {
        let err = parse_suggestions(r#"{"suggestions":"git ch"}"#).unwrap_err();
        assert!(err.contains("a string"), "err={err}");
    }

    #[test]
    fn rejects_garbage_and_scalars() {
        assert!(parse_suggestions("not json at all").is_err());
        assert!(parse_suggestions("42").is_err());
        let err = parse_suggestions(r#"[{"no_name":1}]"#).unwrap_err();
        assert!(err.contains("`name`"), "err={err}");
    }

    /// Empty output is a legitimate "nothing to offer", not an error.
    #[test]
    fn empty_output_yields_an_empty_blob() {
        for empty in ["", "   ", "null"] {
            let blob = parse_suggestions(empty).unwrap();
            assert!(blob.suggestions.is_empty(), "{empty}");
        }
    }

    /// A crashed child's placeholder text (from `run_complete`) can never
    /// masquerade as suggestions.
    #[test]
    fn error_placeholder_fails_to_parse() {
        assert!(parse_suggestions("<error: exited with 101: panic>").is_err());
    }
}
