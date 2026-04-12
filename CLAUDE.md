# CLAUDE.md — inshellisense-rs

## What this is

Pure-Rust port of [Microsoft's inshellisense](https://github.com/microsoft/inshellisense) — IDE-style shell autocomplete. Ships a single binary `is` that wraps your shell with ghost-text suggestions and an interactive popup. Drop-in replacement: same config files, same env vars, same OSC 6973 protocol.

## Build & test

```bash
cargo build --release          # binary at target/release/is
cargo test --release           # 52 tests (35 unit + 17 parity)
cargo clippy --release --all-targets -- -D warnings
```

The binary is ~5.8 MB stripped (3.8 MB of that is the embedded zstd-compressed spec bundle).

## Architecture

- **`src/main.rs`** — CLI arg parsing (manual, no clap). Dispatches to subcommands.
- **`src/pty.rs`** — Main event loop. Uses `libc::forkpty` + `libc::poll` (no portable-pty, no crossterm). Spawns the wrapped shell, proxies I/O, drives the renderer.
- **`src/render/`** — Ghost text (`ghost.rs`), popup TUI (`popup.rs`), hybrid mode (`mod.rs`). All output via `&mut impl Write` — no owned stdout handles.
- **`src/spec/`** — Fig spec model (`model.rs`), parser (`parser.rs`), resolver (`resolver.rs`), lazy-loading registry (`mod.rs`).
- **`src/suggest.rs`** — Suggestion engine. Consumes registry + history, returns ranked `Vec<Suggestion>`.
- **`src/term.rs`** — Headless vt100 terminal tracker. Feeds PTY output through `vt100-ctt` parser to extract the current command text.
- **`src/ansi.rs`** — OSC 6973 stream scanner (prompt-start/end/cwd markers).
- **`src/config.rs`** — TOML config loader. Reads `~/.inshellisenserc`, `~/.config/inshellisense/rc.toml`, `~/.config/insh-rs/rc.toml`. Accepts both camelCase and snake_case field names.
- **`src/parity/`** — Dev-only parity scanner (`cargo run --bin parity-scan`). Compares our output against upstream's binary across 6 categories.

## Spec loading

Specs are lazy-loaded, matching upstream's dynamic-import model:

1. At startup: decompress `specs-data/bundle.json.zst` (3.8 MB → 74 MB), scan top-level JSON keys via `serde_json::RawValue` (~75 ms). No spec values are parsed yet.
2. On first `registry.get("git")`: deserialize just that spec's JSON bytes (~1 ms), cache it.
3. The bundle is regenerated from `specs-data/extras/` + `specs-data/embed/` via a Python one-liner (see commit history). Only the `.zst` is committed.

## Dependencies (29 crates total)

Direct: `vt100-ctt` (headless terminal), `anyhow` (errors), `serde` + `serde_json` (spec JSON), `toml` (config), `libc` (syscalls — poll, forkpty, termios), `unicode-width` (popup column math), `ruzstd` (zstd decompression).

We intentionally avoid: clap (manual arg parsing), portable-pty (raw libc), crossterm (raw libc), rayon (std::thread::scope), dirs (env vars), once_cell (std::LazyLock).

## Key conventions

- **No TUI framework.** All rendering is raw ANSI escape sequences written to stdout. Popup uses SCO save/restore (`\x1b[s`/`\x1b[u`) and repeated `\x1b[1C` padding to match upstream's byte output.
- **Single stdout writer.** Renderers take `&mut impl Write`, never own a `Stdout` handle. The main loop passes its locked stdout through all calls.
- **libc::poll event loop.** Blocks on PTY master fd + stdin fd. Zero CPU when idle. No channels, no reader threads.
- **Signal handlers.** SIGTERM/SIGHUP/SIGINT restore the original termios before exit. The original is saved before `cfmakeraw`.
- **Background engine load.** The spec registry decompresses + indexes on a background thread (~200 ms). Shell prompt appears in ~5 ms; suggestions ready by the time you type.

## Testing

- `tests/parity.rs` — 17-case JSONL-driven corpus testing suggestion output against known-good values.
- `tests/parity/` — Corpus files for the parity scanner (`complete.jsonl`, `render.jsonl`, `cli.txt`).
- `src/parity/` — Scanner binary that runs both our `is` and upstream's `inshellisense` through identical inputs and diffs outputs. Run: `cargo run --bin parity-scan -- --only complete`.

## Common tasks

```bash
# Run the interactive shell
is start                    # default hybrid mode (ghost + popup)
is start --ui popup         # popup only
is start --ui ghost         # ghost text only

# Offline completion
is complete "git ch"        # JSON output (default)
is complete "git ch" --text # ghost-tail text only

# Regenerate the spec bundle (requires specs-data/extras/ populated)
python3 -c "..." | zstd -19 -o specs-data/bundle.json.zst  # see commit history

# Run parity scanner against upstream
cargo run --bin parity-scan -- --only complete --verbose
```

## Resource paths

- `~/.insh-rs/` — runtime resource root (shell integration scripts, init files, version.txt)
- `~/.config/insh-rs/rc.toml` — user config
- `~/.config/insh-rs/specs/*.toml` — user-added specs
- `~/.inshellisenserc` — upstream-compat config (read-only)
