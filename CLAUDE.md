# CLAUDE.md — inshellisense-rs

## What this is

Pure-Rust port of [Microsoft's inshellisense](https://github.com/microsoft/inshellisense) — IDE-style shell autocomplete. Ships a single binary `is` that wraps your shell with ghost-text suggestions and an interactive popup. Reads upstream-compatible config files and env vars, keeps its own canonical config under `inshellisense-rs`, and uses the same OSC 6973 protocol. Cross-platform: Linux, macOS, Windows.

## Build & test

```bash
cargo build --release          # binary at target/release/is (or is.exe on Windows)
cargo test --all-targets       # unit, CLI, and parity-corpus tests
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
- **`src/config.rs`** — TOML config loader. Reads `~/.inshellisenserc`, `~/.config/inshellisense/rc.toml`, and `~/.config/inshellisense-rs/rc.toml` (or the corresponding `%USERPROFILE%` / `%APPDATA%` paths on Windows). Accepts both camelCase and snake_case field names.
- **`src/shell.rs`** — Shell enum (Bash, Zsh, Fish, Pwsh, Powershell, Xonsh, Nu, Cmd on Windows). Detection, spawn targets, init snippets.
- **`src/coreutils.rs`** — Runtime detection of an installed [Coreutils for Windows](https://github.com/microsoft/coreutils) / uutils multi-call binary.
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
3. The bundle is generated from extractor output under `specs-data/extras/` + `specs-data/embed/`; runtime JSON overrides can also come from `INSH_RS_SPECS_DIR` or `[specs].path`.

## Coreutils integration

If a `coreutils` multi-call binary is on `PATH` (Microsoft's [Coreutils for
Windows](https://github.com/microsoft/coreutils), or upstream `uutils/coreutils`
on any platform), it is detected at startup and:

1. A `coreutils` spec is registered whose subcommands are whatever
   `coreutils --list` reports. Each delegates to the same-named spec, so
   `coreutils ls --<TAB>` completes `ls`'s options.
2. Utilities the bundled corpus does not cover (`b2sum`, `numfmt`,
   `sha256sum`, …) get a spec synthesized from their `--help`, which is
   regular clap output.
3. For a utility the corpus *does* cover, the options the installed binary
   actually accepts are merged into the bundled spec — but only after
   verifying that the command on `PATH` is that same binary, by comparing
   `<util> --version` against `coreutils <util> --version`. Fig's `ls` spec is
   BSD-flavored and knows `-a` but not `--all`; a coreutils `ls` accepts both.

Everything is cached under `~/.inshellisense/coreutils/<fingerprint>/`, keyed
by the binary's size and mtime, so the steady-state cost is zero subprocesses.
`INSH_RS_NO_COREUTILS=1` disables detection; `Registry::new_with_options(false)`
does the same in-process (the parity scanner and its tests use it, so results
never depend on what is installed on the host).

## Dependencies (29 crates on Linux/macOS)

Direct: `vt100-ctt` (headless terminal), `anyhow` (errors), `serde` + `serde_json` (spec JSON), `toml` (config), `libc` (POSIX syscalls), `unicode-width` (popup column math), `ruzstd` (zstd decompression). Windows adds `windows-sys` (target-gated).

We intentionally avoid: clap, portable-pty, crossterm, rayon, dirs, once_cell.

## Key conventions

- **No TUI framework.** All rendering is raw ANSI escape sequences written to stdout.
- **Single stdout writer.** Renderers take `&mut impl Write`, never own a `Stdout` handle.
- **Platform-abstracted event loop.** `PtyHandle::poll()` blocks until data arrives. Zero CPU when idle on all platforms (libc::poll on Unix, WaitForMultipleObjects on Windows).
- **Signal/ctrl handlers.** Unix: SIGTERM/SIGHUP/SIGINT restore termios. Windows: SetConsoleCtrlHandler restores console mode while raw input leaves Ctrl-C/Ctrl-Break available to the child shell.
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
| Resource root | `~/.inshellisense/` | `%USERPROFILE%\.inshellisense\` |
| User config | `~/.config/inshellisense-rs/rc.toml` | `%APPDATA%\inshellisense-rs\rc.toml` |
| User TOML specs | `~/.config/inshellisense-rs/specs/*.toml` | `%APPDATA%\inshellisense-rs\specs\*.toml` |
| JSON spec dirs | `INSH_RS_SPECS_DIR`, `[specs].path` | `INSH_RS_SPECS_DIR`, `[specs].path` |
| Upstream compat | `~/.inshellisenserc`, `~/.config/inshellisense/rc.toml` | `%USERPROFILE%\.inshellisenserc`, `%APPDATA%\inshellisense\rc.toml` |
| Coreutils cache | `~/.inshellisense/coreutils/` | `%USERPROFILE%\.inshellisense\coreutils\` |
