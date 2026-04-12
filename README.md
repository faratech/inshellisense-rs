# inshellisense-rs

Cross-platform shell autocomplete — a Rust port of Microsoft's
[inshellisense](https://github.com/microsoft/inshellisense).

Type a partial command, see the best completion appear in grey after the
cursor, press `→` to accept. Same PowerShell PSReadLine feel, implemented
as a PTY wrapper around your existing shell. Linux, macOS, and Windows.

```
$ git ch█eckout        ← grey suggestion ─ press → to accept
```

## Status

**1:1 feature parity with upstream inshellisense.** 1470 specs bundled
(zstd-compressed), lazy-loaded on demand. Pure Rust — no JS runtime, no
Node.js. 29 crates, 5.7 MB binary (vs upstream's 132 MB). Supports
bash, zsh, fish, pwsh, powershell, xonsh, nushell, and cmd.exe (Windows).

| Platform | Status |
|----------|--------|
| Linux | Fully tested |
| macOS | Compiles (POSIX-compatible), needs field testing |
| Windows 10+ | ConPTY implementation complete, needs field testing |

## Why another autocomplete tool

The existing options in bash:

| Tool | Latency | Memory | Suggests new commands? |
|---|---|---|---|
| `bash-completion` | native | native | only what you've typed before |
| [`ble.sh`](https://github.com/akinomyoga/ble.sh) | native | native | history only |
| [`fzf`](https://github.com/junegunn/fzf) | native | small | history only |
| Microsoft [inshellisense](https://github.com/microsoft/inshellisense) | ~100 ms cold start, ~60 MB | Node + V8 | yes, via Fig specs |
| **inshellisense-rs** | **~5 ms prompt, ~220 ms suggestions, ~5.7 MB** | native Rust binary | yes, via Fig specs |

The positioning: **inshellisense's feature set with ripgrep's footprint**.

## Install

### From source

```sh
cargo install --path .
is install              # appends the init snippet to ~/.bashrc
```

Or explicitly:

```sh
cargo build --release
install -m 755 target/release/is ~/.local/bin/is
is install
```

### Try without installing

```sh
exec is start           # drop into a wrapped bash in the current terminal
```

## Usage

```sh
is start                # run wrapped shell (the real thing)
is doctor               # show resolved config + loaded specs
is list-specs           # enumerate loaded commands
is complete 'git ch'    # offline query → prints the suggestion tail
is init bash            # print the init snippet for ~/.bashrc
is install              # write the snippet into ~/.bashrc
```

In the wrapped shell:

| Key | Action |
|---|---|
| `→` / `End` / `Ctrl-E` | Accept the current ghost suggestion |
| Anything else | Forwarded to bash unchanged |

## User-defined specs

Drop TOML files in `~/.config/insh-rs/specs/` to add or override commands.
The schema mirrors the Rust model in [`src/spec/model.rs`](src/spec/model.rs).

```toml
# ~/.config/insh-rs/specs/hello.toml
names = ["hello"]
description = "Say hello"

[[subcommands]]
names = ["world"]
description = "Say hello to the world"

[[subcommands]]
names = ["kitty"]
description = "Say hello to a cat"
```

## Architecture

```
┌─────────────────┐   bytes   ┌────────────────┐
│ user's terminal │──────────▶│ portable-pty   │
│                 │           │ (bash wrapper) │
│                 │◀──────────│                │
└────────┬────────┘   bytes   └───────┬────────┘
         │                            │
         ▼                            ▼
   ┌──────────┐               ┌───────────────┐
   │ renderer │◀──────────────│ vt100 tracker │
   │  (ANSI)  │ suggestion    │ + command mgr │
   └──────────┘  ▲            └───────┬───────┘
                 │                    │ command state
                 │                    ▼
                 │            ┌────────────────┐
                 └────────────│ suggest engine │
                              │  + spec walker │
                              └────────────────┘
```

Cold start: ~5 ms. Release binary: 1.7 MB stripped.

The suggestion engine parses the current command-line state using a
tokenizer and resolver ported from inshellisense, walks a registry of
Fig.Spec-shaped specs, and returns a ranked `Vec<Suggestion>`. The
renderer picks the top suggestion and emits grey-foreground ANSI that
positions at the cursor via save/restore.

## Comparison with inshellisense

insh-rs aims for behavioral parity with Microsoft's inshellisense on the
specs it covers. Where it differs:

| | inshellisense | insh-rs |
|---|---|---|
| Runtime | Node + V8 | native Rust binary |
| Cold start | ~100 ms | ~5 ms |
| Memory | ~60 MB | ~5.7 MB |
| Binary size | 30+ MB (packaged) | 1.7 MB stripped |
| Shell support | bash, zsh, fish, pwsh, nu, xonsh, cmd | bash only |
| Fig spec coverage | ~715 specs via dynamic import | **1470/1470 pure (100.0%)** via the static extractor |
| JS runtime for opaque closures | always on (Node) | **none** — pure Rust static extraction only |
| Ghost-text rendering | yes | yes |
| Right-arrow to accept | yes | yes |

Phase 3 closes the spec coverage gap. Phase 6 closes the opaque-closure
gap via `rquickjs`. Parity testing against an inshellisense oracle lands
in phase 5.

## Roadmap

| Phase | Goal | Status |
|---|---|---|
| 1 | Schema v2 + tokenizer + resolver | ✅ done |
| 2 | `Vec<Suggestion>` output + concurrent multi-generator exec | ✅ done |
| 3 | Extractor tool + spec loader + 715+ spec coverage | ✅ done |
| 4 | `PostProcessKind` DSL + lazy `LoadSpec` + extractor isolation | ✅ done |
| 4.5 | Extractor identifier + property-access resolution | ✅ done |
| 4.6 | Static factory evaluator + template folding → 97.1% coverage | ✅ done |
| 5 | 82-case parity corpus + GitHub Actions CI gate | ✅ done |
| 6.1 | `createVersionedSpec` handler → 97.6% | ✅ done |
| 6.2 | `ProjectFile` + `FileExistsThen` generators + shorthand fix → 99.86% pure / 100% loaded | ✅ done |
| 6.6 | Parity expansion to 111 cases + CI threshold tightening | ✅ done |
| ~~6~~ | ~~`rquickjs` JS runtime for opaque closures~~ | dropped — pure Rust path reached 99.86% |

## Credits

insh-rs stands on the shoulders of:

- **[inshellisense](https://github.com/microsoft/inshellisense)** by
  Microsoft — the algorithmic design, the shell integration protocol, the
  parser/resolver reference implementation.
- **[@withfig/autocomplete](https://github.com/withfig/autocomplete)** by
  Hercules Labs (Fig, now AWS) — the ~715 completion specs that make this
  useful on real commands.
- **[bash-preexec](https://github.com/rcaloras/bash-preexec)** by Ryan
  Caloras — the precmd/preexec plumbing for the shell integration.

All three are MIT licensed. See [NOTICE](NOTICE) for full attribution and
[LICENSES/](LICENSES/) for upstream license texts.

**insh-rs is not affiliated with Microsoft, Amazon, Fig, or Hercules Labs.**
"inshellisense" is a trademark of Microsoft Corporation.

## License

MIT — see [LICENSE](LICENSE).
