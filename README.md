# insh-rs

Grey ghost-text shell autocomplete — a Rust port of Microsoft's
[inshellisense](https://github.com/microsoft/inshellisense).

Type a partial command, see the best completion appear in grey after the
cursor, press `→` to accept. Same PowerShell PSReadLine feel, implemented
as a PTY wrapper around your existing bash so there's nothing to learn.

```
$ git ch█eckout        ← grey suggestion ─ press → to accept
```

## Status

**Alpha — phase 1 of 6 complete.** Works end-to-end on 5 hand-ported
commands (git, docker, cargo, systemctl, ssh). Full `@withfig/autocomplete`
spec coverage (715 commands) lands in phase 3. See
[CHANGELOG.md](CHANGELOG.md) for the phase plan.

## Why another autocomplete tool

The existing options in bash:

| Tool | Latency | Memory | Suggests new commands? |
|---|---|---|---|
| `bash-completion` | native | native | only what you've typed before |
| [`ble.sh`](https://github.com/akinomyoga/ble.sh) | native | native | history only |
| [`fzf`](https://github.com/junegunn/fzf) | native | small | history only |
| Microsoft [inshellisense](https://github.com/microsoft/inshellisense) | ~100 ms cold start, ~60 MB | Node + V8 | yes, via Fig specs |
| **insh-rs** | **~5 ms cold start, ~10 MB** | native Rust binary | yes, via Fig specs |

The positioning: **inshellisense's feature set with ripgrep's footprint**.

## Install

### From source

```sh
cargo install --path .
insh install              # appends the init snippet to ~/.bashrc
```

Or explicitly:

```sh
cargo build --release
install -m 755 target/release/insh ~/.local/bin/insh
insh install
```

### Try without installing

```sh
exec insh start           # drop into a wrapped bash in the current terminal
```

## Usage

```sh
insh start                # run wrapped shell (the real thing)
insh doctor               # show resolved config + loaded specs
insh list-specs           # enumerate loaded commands
insh complete 'git ch'    # offline query → prints the suggestion tail
insh init bash            # print the init snippet for ~/.bashrc
insh install              # write the snippet into ~/.bashrc
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
| Memory | ~60 MB | ~10 MB |
| Binary size | 30+ MB (packaged) | 1.7 MB stripped |
| Shell support | bash, zsh, fish, pwsh, nu, xonsh, cmd | bash only (phase 1) |
| Fig spec coverage | ~715 specs via dynamic import | 5 curated specs (phase 1) → 715 via extractor (phase 3+) |
| JS runtime for opaque closures | always on (Node) | opt-in via `--features js` (phase 6) |
| Ghost-text rendering | yes | yes |
| Right-arrow to accept | yes | yes |

Phase 3 closes the spec coverage gap. Phase 6 closes the opaque-closure
gap via `rquickjs`. Parity testing against an inshellisense oracle lands
in phase 5.

## Roadmap

| Phase | Goal | Status |
|---|---|---|
| 1 | Schema v2 + tokenizer + resolver | ✅ done |
| 2 | `Vec<Suggestion>` output + concurrent multi-generator exec | pending |
| 3 | Extractor tool + msgpack loader + 715-spec coverage | pending |
| 4 | `PostProcessKind` DSL + lazy `LoadSpec` + `parserDirectives` | pending |
| 5 | Parity corpus + CI gate vs inshellisense | pending |
| 6 | `rquickjs` JS runtime for opaque closures | pending |

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
