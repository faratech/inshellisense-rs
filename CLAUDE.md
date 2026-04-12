# CLAUDE.md — inshellisense-rs

## What this is

Pure-Rust port of [Microsoft's inshellisense](https://github.com/microsoft/inshellisense) — IDE-style shell autocomplete. Ships a single binary `is` that wraps your shell with ghost-text suggestions and an interactive popup. Drop-in replacement: same config files, same env vars, same OSC 6973 protocol. Cross-platform: Linux, macOS, Windows.

## Build & test

```bash
cargo build --release          # binary at target/release/is (or is.exe on Windows)
cargo test --release           # 52 tests (35 unit + 17 parity)
cargo clippy --release --all-targets -- -D warnings
```

The binary is ~5.8 MB stripped (3.8 MB of that is the embedded zstd-compressed spec bundle).

## Architecture

- **`src/main.rs`** — CLI arg parsing (manual, no clap). Dispatches to subcommands.
- **`src/pty.rs`** — Main event loop. Uses `platform::PtyHandle` trait for PTY I/O. Spawns the wrapped shell, proxies I/O, drives the renderer.
- **`src/platform/`** — Cross-platform PTY abstraction:
  - `mod.rs` — `PtyHandle` trait + shared helpers (find_on_path, term_size, raw mode, signal handlers)
  - `unix.rs` — POSIX implementation (forkpty, poll, termios). Linux + macOS.
  - `windows.rs` — ConPTY implementation (CreatePseudoConsole, WaitForMultipleObjects, SetConsoleMode). Windows 10+.
- **`src/render/`** — Ghost text (`ghost.rs`), popup TUI (`popup.rs`), hybrid mode (`mod.rs`). All output via `&mut impl Write` — no owned stdout handles.
- **`src/spec/`** — Fig spec model (`model.rs`), parser (`parser.rs`), resolver (`resolver.rs`), lazy-loading registry (`mod.rs`).
- **`src/suggest.rs`** — Suggestion engine. Consumes registry + history, returns ranked `Vec<Suggestion>`.
- **`src/term.rs`** — Headless vt100 terminal tracker. Feeds PTY output through `vt100-ctt` parser to extract the current command text.
- **`src/ansi.rs`** — OSC 6973 stream scanner (prompt-start/end/cwd markers).
- **`src/config.rs`** — TOML config loader. Reads `~/.inshellisenserc`, `~/.config/inshellisense/rc.toml`, `~/.config/insh-rs/rc.toml` (or `%APPDATA%` on Windows). Accepts both camelCase and snake_case field names.
- **`src/shell.rs`** — Shell enum (Bash, Zsh, Fish, Pwsh, Powershell, Xonsh, Nu, Cmd on Windows). Detection, spawn targets, init snippets.
- **`src/parity/`** — Dev-only parity scanner (`cargo run --bin parity-scan`).

## Platform support

| Platform | PTY | Event loop | Terminal control | Status |
|----------|-----|------------|-----------------|--------|
| Linux | forkpty(3) | poll(2) | termios | Fully tested |
| macOS | forkpty(3) | poll(2) | termios | Compiles, needs testing |
| Windows | ConPTY | WaitForMultipleObjects | SetConsoleMode | Field-tested (PowerShell + Git Bash in Windows Terminal) |

Windows uses `windows-sys` crate (target-gated, zero impact on Linux/macOS). Supports Windows 10 1809+ (ConPTY requirement).

## Shells

| Shell | Linux/macOS | Windows |
|-------|-------------|---------|
| Bash | ✓ | ✓ (Git Bash — auto-discovered) |
| Zsh | ✓ | — |
| Fish | ✓ | ✓ |
| Pwsh | ✓ | ✓ |
| PowerShell | ✓ | ✓ |
| Xonsh | ✓ | ✓ |
| Nushell | ✓ | ✓ |
| Cmd | — | ✓ (PROMPT-based OSC markers) |

## Spec loading

Specs are lazy-loaded, matching upstream's dynamic-import model:

1. At startup: decompress `specs-data/bundle.json.zst` (3.8 MB → 74 MB), scan top-level JSON keys via `serde_json::RawValue` (~75 ms). No spec values are parsed yet.
2. On first `registry.get("git")`: deserialize just that spec's JSON bytes (~1 ms), cache it.
3. The bundle is regenerated from `specs-data/extras/` + `specs-data/embed/` via a Python one-liner (see commit history). Only the `.zst` is committed.

## Dependencies (29 crates on Linux/macOS)

Direct: `vt100-ctt` (headless terminal), `anyhow` (errors), `serde` + `serde_json` (spec JSON), `toml` (config), `libc` (POSIX syscalls), `unicode-width` (popup column math), `ruzstd` (zstd decompression). Windows adds `windows-sys` (target-gated).

We intentionally avoid: clap, portable-pty, crossterm, rayon, dirs, once_cell.

## Key conventions

- **No TUI framework.** All rendering is raw ANSI escape sequences written to stdout.
- **Single stdout writer.** Renderers take `&mut impl Write`, never own a `Stdout` handle.
- **Platform-abstracted event loop.** `PtyHandle::poll()` blocks until data arrives. Zero CPU when idle on all platforms (libc::poll on Unix, WaitForMultipleObjects on Windows).
- **Signal/ctrl handlers.** Unix: SIGTERM/SIGHUP/SIGINT restore termios. Windows: SetConsoleCtrlHandler restores console mode.
- **Background engine load.** Registry decompresses + indexes on a background thread (~200 ms).
- **Windows stdin via ReadConsoleInputW.** Reads raw KEY_EVENTs and converts virtual key codes to VT sequences. Never blocks on line mode. Synthetic vk=0 events from ENABLE_VIRTUAL_TERMINAL_INPUT are filtered.
- **Windows PTY output via PeekNamedPipe.** ConPTY signals the pipe handle even without data; plain ReadFile would deadlock the event loop.
- **Deferred PromptEnd anchor.** ConPTY sends OSC 6973 markers in a separate chunk before the screen-paint bytes. The anchor is applied after the next batch of bytes updates the vt100 screen.
- **Windows exit via RIS + process::exit.** Matches upstream: write `\x1bc` (Reset to Initial State), restore console mode, then `process::exit(0)`. ClosePseudoConsole deadlocks in Windows Terminal.

## Common tasks

```bash
# Run the interactive shell
is start                    # default hybrid mode (ghost + popup)
is start --ui popup         # popup only
is start --shell bash       # force a specific shell

# Offline completion
is complete "git ch"        # JSON output (default, matches upstream schema)
is complete "git ch" --text # ghost-tail text only

# Windows: build + run (native)
cargo build --release
.\target\release\is.exe start
.\target\release\is.exe start --shell cmd  # Windows CMD

# Windows: cross-compile from WSL (requires cargo-xwin)
cargo xwin build --release --target aarch64-pc-windows-msvc  # ARM64
cargo xwin build --release --target x86_64-pc-windows-msvc   # x64
```

## Resource paths

| | Unix | Windows |
|---|---|---|
| Resource root | `~/.insh-rs/` | `%USERPROFILE%\.insh-rs\` |
| User config | `~/.config/insh-rs/rc.toml` | `%APPDATA%\insh-rs\rc.toml` |
| User specs | `~/.config/insh-rs/specs/*.toml` | `%APPDATA%\insh-rs\specs\*.toml` |
| Upstream compat | `~/.inshellisenserc` | `%USERPROFILE%\.inshellisenserc` |
