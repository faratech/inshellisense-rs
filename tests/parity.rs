//! Parity test harness — hand-crafted unit cases plus a JSONL-driven
//! corpus loaded from `tests/parity-corpus.jsonl`.
//!
//! Each corpus entry supports these assertion keys (any combination,
//! all must pass for the case to count as a pass):
//!
//! * `line`   — command line input (required)
//! * `cwd`    — optional working directory override (default ".")
//! * `expect_tail` — `engine.suggest()` must return exactly this tail
//! * `expect_top_name` — `blob.first().name` must equal this
//! * `expect_contains_name` — these names must all appear in the blob
//! * `expect_blob_min` — blob must contain at least N suggestions
//! * `expect_none` — `engine.suggest()` must return None
//!
//! The `corpus_drives_parity_above_threshold` test loads the JSONL at
//! runtime, runs every case, prints a summary, and fails if the pass
//! rate drops below the configured threshold (currently 95%).

use inshellisense_rs::{
    shell::Shell,
    spec::{Opt, Registry, Subcommand},
    suggest::Engine,
};
use serde::Deserialize;
use std::sync::OnceLock;

/// Build a registry with the extras corpus loaded.
///
/// Every test used to depend on a `set_var("INSH_RS_SPECS_DIR", ..)` buried
/// inside a lazy singleton: whether a sibling test saw the extras came down
/// to which test touched the singleton first, and `set_var` races the other
/// threads `cargo test` runs concurrently. Load the directory explicitly.
fn build_registry() -> Registry {
    let extras_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("specs-data")
        .join("extras");
    // `false`: never probe a host coreutils install — its specs would make
    // these comparisons depend on the machine the tests run on.
    let mut registry = Registry::new_with_options(false);
    registry.load_spec_dir(&extras_dir);
    registry
}

/// Shared engine built once per test binary. The parity corpus runs 100+
/// cases, and without this each case would rebuild a Registry from scratch —
/// walking 1400+ spec files from disk for every case.
fn shared_engine() -> &'static Engine {
    static CELL: OnceLock<Engine> = OnceLock::new();
    CELL.get_or_init(|| Engine::new(build_registry(), Vec::new()))
}

fn top_suggestion(line: &str) -> Option<String> {
    let registry = build_registry();
    let engine = Engine::new(registry, Vec::new());
    engine.suggest(line, ".")
}

fn top_name(line: &str) -> Option<String> {
    let registry = build_registry();
    let engine = Engine::new(registry, Vec::new());
    let blob = engine.suggest_blob(line, ".");
    blob.first().map(|s| s.name.clone())
}

#[test]
fn git_subcommand_completion() {
    // Shortest subcommand starting with "ch" is "checkout".
    assert_eq!(top_suggestion("git ch"), Some("eckout".to_string()));
}

#[test]
fn git_subcommand_longer_prefix() {
    assert_eq!(top_suggestion("git che"), Some("ckout".to_string()));
}

#[test]
fn git_branch_suggestion() {
    // "br" → "anch".
    assert_eq!(top_suggestion("git br"), Some("anch".to_string()));
}

#[test]
fn docker_has_many_subcommands() {
    // Sanity check on the extracted docker spec: a rich subcommand tree.
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("docker ", ".")
    };
    assert!(
        blob.len() >= 40,
        "expected ≥40 docker subcommands, got {}",
        blob.len()
    );
}

#[test]
fn docker_run_has_detach() {
    // After `docker run `, option suggestions should include --detach.
    // This exercises the PropertyAccessExpression resolver: the run
    // subcommand comes from sharedCommands.run in upstream docker.ts.
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("docker run ", ".")
    };
    assert!(
        blob.iter().any(|s| s.name == "--detach"),
        "expected --detach in docker run options, got {:?}",
        blob.iter().take(10).map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn option_value_binding_skips_path_arg() {
    // `git -C /tmp st` — the `/tmp` is consumed as the arg to `-C`, then
    // we're back at git level and "st" matches a `st*` subcommand.
    // The extracted git spec has stage, stash, status — any of those
    // proves option-value binding worked.
    let top = top_suggestion("git -C /tmp st");
    assert!(
        top.is_some(),
        "option-value binding failed: no suggestion for 'git -C /tmp st'"
    );
    let t = top.unwrap();
    assert!(
        ["age", "ash", "atus"].contains(&t.as_str()),
        "expected stage/stash/status tail, got {t:?}"
    );
}

#[test]
fn long_option_equals_splits() {
    // `cargo build --target=wasm32` — tokenizer must split at `=`.
    // After `--target=wasm32` with no trailing space, we're completing the
    // `wasm32` partial. No spec match expected, but also no crash.
    let _ = top_name("cargo build --target=wasm32");
}

#[test]
fn unknown_command_returns_none() {
    assert_eq!(top_suggestion("definitely-not-a-real-command fo"), None);
}

#[test]
fn empty_line_returns_none() {
    assert_eq!(top_suggestion(""), None);
    assert_eq!(top_suggestion("   "), None);
}

#[test]
fn cargo_subcommand_completion() {
    // cargo upstream defines a `b` alias for build, so `cargo b` matches
    // exactly and yields no tail. Use `bu` to force a longer-match.
    assert_eq!(top_suggestion("cargo bu"), Some("ild".to_string()));
}

#[test]
fn systemctl_start_status() {
    // `systemctl sta` — shortest of start/status/stop is "start" (start/stop/status all length 5/6).
    let got = top_suggestion("systemctl sta");
    assert!(
        got.as_deref() == Some("rt") || got.as_deref() == Some("tus"),
        "expected 'start' or 'status' tail, got {got:?}"
    );
}

#[test]
fn dash_dash_marks_raw_tokens() {
    // After `git log -- file1 `, `file1` should be raw (positional) —
    // but `git log` has no args defined, so we just confirm no crash.
    let _ = top_name("git log -- file1");
}

#[test]
fn completion_with_trailing_space_offers_subcommands() {
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("git ", ".")
    };
    // Expect at least 10 git subcommands in the blob.
    assert!(
        blob.len() >= 10,
        "expected ≥10 git subcommand suggestions, got {}",
        blob.len()
    );
}

#[test]
fn posix_short_flag_prefers_exact_case_match() {
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("ls -l", ".")
    };
    assert_eq!(blob.first().map(|s| s.name.as_str()), Some("-l"));
}

#[test]
fn exact_case_option_prefix_beats_wrong_case_priority() {
    let mut registry = Registry::default();
    let mut spec = Subcommand::new("caseprobe");
    spec.options = vec![
        Opt {
            names: vec!["-L".into()],
            priority: Some(100),
            ..Default::default()
        },
        Opt {
            names: vec!["-l".into()],
            priority: Some(1),
            ..Default::default()
        },
    ];
    registry.insert(spec);
    let engine = Engine::new(registry, Vec::new());
    let blob = engine.suggest_blob("caseprobe -l", ".");
    assert_eq!(blob.first().map(|s| s.name.as_str()), Some("-l"));
}

#[test]
fn powershell_ls_uses_get_childitem_options() {
    let registry = build_registry();
    let mut engine = Engine::new(registry, Vec::new());
    engine.set_shell(Shell::Pwsh);

    let blob = engine.suggest_blob("ls -", ".");
    let names: std::collections::HashSet<&str> = blob.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains("-Recurse"),
        "expected PowerShell -Recurse in {names:?}"
    );
    assert!(
        names.contains("-Force"),
        "expected PowerShell -Force in {names:?}"
    );
    assert!(
        !names.contains("-l"),
        "PowerShell ls should not use GNU ls flags: {names:?}"
    );
}

#[test]
fn requires_separator_empty_value_suggests_values() {
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("eza --color-scale=", ".")
    };
    let names: std::collections::HashSet<&str> = blob.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains("all"), "expected all in {names:?}");
    assert!(names.contains("age"), "expected age in {names:?}");
}

#[test]
fn depends_on_filters_options_until_dependency_is_present() {
    let registry = build_registry();
    let engine = Engine::new(registry, Vec::new());
    let without = engine.suggest_blob("cp -", ".");
    assert!(
        !without
            .iter()
            .any(|s| ["-H", "-L", "-P"].contains(&s.name.as_str())),
        "dependency-gated options leaked: {:?}",
        without.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    let with = engine.suggest_blob("cp -R -", ".");
    assert!(
        with.iter().any(|s| s.name == "-H"),
        "expected -H after -R, got {:?}",
        with.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn options_must_precede_arguments_suppresses_options_after_arg() {
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("nc example.com -", ".")
    };
    assert!(
        !blob.iter().any(|s| s.name.starts_with('-')),
        "expected no options after positional arg, got {:?}",
        blob.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn alias_completion_uses_original_token_and_active_segment() {
    let registry = build_registry();
    let mut engine = Engine::new(registry, Vec::new());
    let mut aliases = std::collections::HashMap::new();
    aliases.insert("g".to_string(), "git".to_string());
    aliases.insert("gs".to_string(), "git status".to_string());
    engine.set_aliases(aliases);

    let exact = engine.suggest_blob("g", ".");
    assert_eq!(exact.first().map(|s| s.name.as_str()), Some("g"));

    let expanded = engine.suggest_blob("echo ok && gs ", ".");
    assert!(
        expanded.iter().any(|s| s.name == "--short"),
        "expected git status options after alias expansion, got {:?}",
        expanded
            .iter()
            .take(10)
            .map(|s| &s.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn cargo_package_json_object_generator_suggests_packages() {
    let registry = build_registry();
    let engine = Engine::new(registry, Vec::new());
    let cwd = env!("CARGO_MANIFEST_DIR");
    let blob = engine.suggest_blob("cargo test --package i", cwd);
    assert!(
        blob.iter().any(|s| s.name == "inshellisense-rs"),
        "expected package name in {:?}",
        blob.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

// ---- phase 3: extractor-produced specs ----

#[test]
fn extracted_find_has_options() {
    // find is an extracted pure-data spec — option -E should be discoverable.
    let blob = {
        let registry = build_registry();
        let engine = Engine::new(registry, Vec::new());
        engine.suggest_blob("find -", ".")
    };
    assert!(
        blob.iter().any(|s| s.name == "-E"),
        "expected -E in find options, got {:?}",
        blob.iter().take(10).map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn extracted_grep_count_option() {
    // grep is extracted; --count is one of its common options.
    assert_eq!(top_suggestion("grep --cou"), Some("nt".to_string()));
}

// ---- JSONL-driven corpus ----

#[derive(Deserialize, Debug)]
struct CorpusCase {
    line: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    expect_tail: Option<String>,
    #[serde(default)]
    expect_top_name: Option<String>,
    #[serde(default)]
    expect_contains_name: Option<Vec<String>>,
    #[serde(default)]
    expect_blob_min: Option<usize>,
    #[serde(default)]
    expect_none: Option<bool>,
}

fn run_case(case: &CorpusCase) -> Result<(), String> {
    let engine = shared_engine();
    let cwd = case.cwd.as_deref().unwrap_or(".");

    if let Some(tail) = &case.expect_tail {
        let got = engine.suggest(&case.line, cwd);
        if got.as_deref() != Some(tail.as_str()) {
            return Err(format!(
                "expect_tail={tail:?} got={got:?} for line={:?}",
                case.line
            ));
        }
    }

    if case.expect_none == Some(true) {
        let got = engine.suggest(&case.line, cwd);
        if got.is_some() {
            return Err(format!(
                "expect_none=true got={got:?} for line={:?}",
                case.line
            ));
        }
    }

    if case.expect_top_name.is_some()
        || case.expect_contains_name.is_some()
        || case.expect_blob_min.is_some()
    {
        let blob = engine.suggest_blob(&case.line, cwd);

        if let Some(top) = &case.expect_top_name {
            match blob.first() {
                Some(s) if s.name == *top => {}
                Some(s) => {
                    return Err(format!(
                        "expect_top_name={top:?} got top={:?} for line={:?}",
                        s.name, case.line
                    ));
                }
                None => {
                    return Err(format!(
                        "expect_top_name={top:?} got empty blob for line={:?}",
                        case.line
                    ));
                }
            }
        }

        if let Some(needed) = &case.expect_contains_name {
            let names: std::collections::HashSet<&str> =
                blob.iter().map(|s| s.name.as_str()).collect();
            let missing: Vec<&str> = needed
                .iter()
                .map(|s| s.as_str())
                .filter(|n| !names.contains(n))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "expect_contains_name missing {missing:?} for line={:?} (blob len {})",
                    case.line,
                    blob.len()
                ));
            }
        }

        if let Some(min) = case.expect_blob_min
            && blob.len() < min
        {
            return Err(format!(
                "expect_blob_min={min} got blob len {} for line={:?}",
                blob.len(),
                case.line
            ));
        }
    }

    Ok(())
}

fn load_corpus() -> Vec<CorpusCase> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("parity-corpus.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty() && !l.starts_with("//"))
        .map(|(i, l)| {
            serde_json::from_str::<CorpusCase>(l)
                .unwrap_or_else(|e| panic!("corpus line {}: {e}\n  {l}", i + 1))
        })
        .collect()
}

#[test]
fn corpus_drives_parity_above_threshold() {
    const THRESHOLD_PCT: f64 = 95.0;
    let cases = load_corpus();
    let total = cases.len();
    let mut passed = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();

    for case in &cases {
        match run_case(case) {
            Ok(()) => passed += 1,
            Err(e) => failures.push((case.line.clone(), e)),
        }
    }

    let pct = (passed as f64) / (total as f64) * 100.0;
    println!("\n=== parity corpus ===");
    println!("  {passed}/{total} cases pass ({pct:.1}%)");
    println!("  threshold: {THRESHOLD_PCT}%");
    if !failures.is_empty() {
        println!("  failures:");
        for (line, err) in &failures {
            println!("    {line:?}: {err}");
        }
    }

    assert!(
        pct >= THRESHOLD_PCT,
        "parity {pct:.1}% below threshold {THRESHOLD_PCT}% ({passed}/{total})"
    );
}

#[test]
fn loaded_spec_count_at_least_20() {
    // Sanity check: the embedded essentials + curated fallbacks should
    // give us at least 20 commands out of the box. The full 1000+ specs
    // live under specs-data/extras/ and are loaded via INSH_RS_SPECS_DIR
    // or a CI-published tarball — not counted here.
    let registry = build_registry();
    assert!(
        registry.len() >= 20,
        "expected ≥20 specs loaded from embed + curated, got {}",
        registry.len()
    );
}
