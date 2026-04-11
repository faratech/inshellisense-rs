# Changelog

All notable changes to insh-rs are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Phase 2 — Suggestion engine polish + parity test harness

- **Added** `src/lib.rs` exposing modules for integration testing.
  Binary stays thin (`src/main.rs` just imports from the crate).
- **Added** `insh complete --json` flag emitting the full ranked
  `Vec<Suggestion>` as pretty JSON for inspection and parity testing.
  Also added `--cwd` override on the `complete` command.
- **Added** concurrent multi-generator execution in
  `generator::suggestions_for_arg`. Args with 2+ generators fan out
  on `std::thread::scope`; 0-1 generators stay on the caller's thread.
- **Added** `accepted_option_tokens` field on `ResolveResult` — the
  resolver now tracks option tokens the user has already typed so
  the suggest engine can respect `exclusive_on` and non-repeatable
  option filtering.
- **Added** `render::pick_top(&[Suggestion], &str) -> Option<String>`
  helper that picks the top suggestion and returns its tail relative
  to the current partial (used by the ghost-text path).
- **Added** `tests/parity.rs` — 12-case hand-crafted parity harness
  covering subcommand completion, option-value binding, `--foo=bar`
  splitting, `--` raw markers, trailing-space subcommand offering,
  unknown-command graceful handling. Scaffold for phase 5's full
  500-case corpus.
- **Changed** `suggest.rs` now filters out options whose names appear
  in `accepted_option_tokens` (unless `is_repeatable`) and options
  whose `exclusive_on` set intersects the accepted tokens.

Validation: `cargo test` passes **26 tests** (14 unit + 12 parity).
Release binary: 1.8 MB.

### Phase 1 — Schema v2 + tokenizer + resolver

- **Added** `src/spec/` module tree mirroring `@withfig/autocomplete-types`:
  - `model.rs` — `Subcommand`, `Opt`, `Arg`, `Generator`, `Suggestion`,
    `LoadSpec`, `ParserDirectives`, `PostProcess`, `PostProcessKind`,
    `ScriptInput`, `CacheSpec`, `FilterStrategy`, `Repeatable`,
    `SuggestionType`, `Template`.
  - `parser.rs` — port of inshellisense's `lex()` state machine from
    `src/runtime/parser.ts`. Handles `--foo=bar` splitting, combined
    short flags, unclosed quotes, pipe/redirect last-segment extraction,
    and bare `--` raw-mode marking (one improvement beyond upstream).
  - `resolver.rs` — port of `runSubcommand` / `runArg` / `runOption` from
    `src/runtime/runtime.ts`. Threads persistent options through
    recursion, binds option values, tracks positional arg position,
    handles variadic args and name aliases.
  - `filter.rs` — case-insensitive prefix and fuzzy filter strategies.
  - `mod.rs` — `Registry` with TOML user-override loader.
- **Added** ergonomic constructors `Subcommand::new`, `Opt::new`, plus
  `matches()` helpers for name-alias lookups.
- **Added** unit tests for tokenizer edge cases (combined shorts,
  `--foo=bar`, unclosed quotes, `--` marker, quoted strings, pipes).
- **Changed** `src/suggest.rs` now returns `Option<String>` via the new
  resolver path plus a new `suggest_blob(line, cwd) -> Vec<Suggestion>`
  method for structured output (used by phase 2's `--json` flag).
- **Changed** `src/generator.rs` now dispatches on the new `Generator`
  enum (`Script { input, split_on, post_process, timeout_ms, cache }`,
  `Template`, `Glob`, `Custom { fn_id }`). `PostProcessKind` DSL hooks
  exist but are forwarded raw until phase 4.
- **Changed** `src/curated.rs` slimmed to 5 representative specs
  (git with persistent `-C <path>` option, docker, cargo, systemctl,
  ssh). The other 10 curated specs from the phase 0 prototype come back
  via the extractor in phase 3.
- **Removed** `src/spec.rs` (replaced by `src/spec/` module tree).

Validation: 14 unit tests pass, `insh complete 'git -C /tmp st'` correctly
suggests `ash` (option-value binding works), release binary is 1.7 MB,
PTY ghost text smoke test passes.

### Phase 0 — Initial MVP

- Working PTY-wrapped bash with `portable-pty`.
- Headless VT tracking via `vt100-ctt` with OSC 6973 prompt markers
  mirroring inshellisense's shell integration protocol.
- Grey ghost-text rendering with save/restore cursor ANSI.
- Right-Arrow / End / Ctrl-E to accept the current suggestion.
- 10 hand-ported curated specs plus a TOML user-override loader.
- Shell command generators with per-(cwd, script) caching.
- `boa_engine` feature-gated scaffold for phase 6's JS escape hatch.
- `insh init` / `insh install` for bashrc integration.
- Vendored `shell/shellIntegration.bash` and `shell/bash-preexec.sh`
  with their original copyright headers preserved.
