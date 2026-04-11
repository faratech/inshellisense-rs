//! Phase-2 parity test scaffold.
//!
//! Hand-crafted cases covering the correctness basics that phase 1+2
//! should handle: subcommand completion, option completion, option-value
//! binding, aliases, `--foo=bar` splitting, and the `--` raw marker.
//!
//! Phase 5 expands this into a 500-case corpus generated from
//! inshellisense's own output. For now these are the smoke cases we
//! check on every commit.

use insh_rs::{spec::Registry, suggest::Engine};

fn top_suggestion(line: &str) -> Option<String> {
    let registry = Registry::new_with_defaults();
    let engine = Engine::new(registry, Vec::new());
    engine.suggest(line, ".")
}

fn top_name(line: &str) -> Option<String> {
    let registry = Registry::new_with_defaults();
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
fn docker_run_has_options() {
    // After `docker run `, option suggestions should include --detach etc.
    let blob = {
        let registry = Registry::new_with_defaults();
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
    // we're back at git level and "st" matches "stash" (or "status" or
    // "start" — whichever is shortest).
    let top = top_suggestion("git -C /tmp st");
    assert!(
        top.is_some(),
        "option-value binding failed: no suggestion for 'git -C /tmp st'"
    );
    let t = top.unwrap();
    assert!(
        ["ash", "atus"].contains(&t.as_str()),
        "expected 'stash' or 'status' tail, got {t:?}"
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
    assert_eq!(top_suggestion("cargo b"), Some("uild".to_string()));
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
        let registry = Registry::new_with_defaults();
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

// ---- phase 3: extractor-produced specs ----

#[test]
fn extracted_find_has_options() {
    // find is an extracted pure-data spec — option -E should be discoverable.
    let blob = {
        let registry = Registry::new_with_defaults();
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

#[test]
fn loaded_spec_count_at_least_20() {
    // Sanity check: the embedded essentials + curated fallbacks should
    // give us at least 20 commands out of the box. The full 1000+ specs
    // live under specs-data/extras/ and are loaded via INSH_RS_SPECS_DIR
    // or a CI-published tarball — not counted here.
    let registry = Registry::new_with_defaults();
    assert!(
        registry.len() >= 20,
        "expected ≥20 specs loaded from embed + curated, got {}",
        registry.len()
    );
}
