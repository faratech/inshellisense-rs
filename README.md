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

**Close behavioral parity with upstream inshellisense**, validated against the
upstream binary across the `complete` suggestion engine, CLI surface, and shell
init. The checked-in parity corpus passes 117/117 cases; `cargo test` runs 231
tests. 1470 specs are bundled (zstd-compressed), lazy-loaded on demand, and
extracted statically; offline gaps that would need live process state or opaque
JS closures are handled with Rust-native generators or documented as runtime
boundaries. Pure Rust — no JS runtime, no Node.js. 28 crates. Supports bash,
zsh, fish, pwsh, powershell, xonsh, nushell, and cmd.exe (Windows).

If an installed [coreutils](#coreutils) build is on `PATH` — Microsoft's
Coreutils for Windows, or upstream `uutils/coreutils` — its commands are
detected and completed too.

| Platform | Status |
|----------|--------|
| Linux | Fully tested |
| macOS | Compiles (POSIX-compatible), needs field testing |
| Windows 10+ | Field-tested (ConPTY, PowerShell + Git Bash in Windows Terminal). The console input layer was reworked recently and is cross-compile-checked and unit-tested, but that rework has not been run on Windows hardware. |

### Footprint

Measured on Linux x86-64, release build:

| | |
|---|---|
| Binary | 6.1 MB (3.8 MB of that is the embedded zstd spec bundle) |
| Resident memory, wrapped shell idle | ~94 MB |
| Peak memory, during startup | ~216 MB |
| `is complete 'git ch'` (cold, one-shot) | ~220 ms |
| Wrapped shell prompt | appears immediately; the spec registry loads on a background thread |

Memory is dominated by the spec index: the 3.8 MB bundle decompresses to 78 MB
of JSON that stays resident so individual specs can be deserialized on demand.
That is a deliberate trade for a ~1 ms per-spec lookup, and it is the obvious
thing to attack if you want this smaller.

## Why another autocomplete tool

The existing options in bash:

| Tool | Suggests new commands? | Runtime |
|---|---|---|
| `bash-completion` | only what you've typed before | shell |
| [`ble.sh`](https://github.com/akinomyoga/ble.sh) | history only | shell |
| [`fzf`](https://github.com/junegunn/fzf) | history only | native |
| Microsoft [inshellisense](https://github.com/microsoft/inshellisense) | yes, via Fig specs | Node + V8 |
| **inshellisense-rs** | yes, via Fig specs | native Rust binary |

The positioning: inshellisense's feature set without the Node runtime.

## Install

### From source

```sh
cargo install --path .
is init bash --install-rc   # appends the bash init snippet to ~/.bashrc
```

`--install-rc` can write the snippet for `bash`, `zsh`, `fish`, and `xonsh`.
`pwsh`, `powershell`, and `nu` resolve their profile path at runtime, so for
those run `is init <shell>` and paste the printed line into your profile.

Or use the legacy bash auto-start wrapper:

```sh
cargo build --release
install -m 755 target/release/is ~/.local/bin/is
is install              # appends an exec wrapper to ~/.bashrc
```

### Try without installing

```sh
exec is start           # drop into a wrapped bash in the current terminal
```

## Usage

```sh
is start                    # run wrapped shell (the real thing)
is start --ui popup         # popup UI instead of the default hybrid
is start --shell zsh        # wrap a specific shell
is doctor                   # health checks + resolved configuration
is specs list               # enumerate loaded commands
is complete 'git ch'        # offline query → JSON (upstream's schema)
is complete 'git ch' --text # offline query → just the ghost-text tail
is init bash                # print the init snippet for ~/.bashrc
is init bash --install-rc   # write the bash init snippet into ~/.bashrc
is reinit                   # regenerate init files and re-unpack resources
is uninstall                # remove cached resources (keeps user config)
is install                  # legacy: auto-start wrapper in ~/.bashrc
```

`is start` exits with the wrapped shell's exit status, so it composes in
scripts: `is start --shell bash <<< 'exit 42'` exits 42.

Arguments a subcommand does not implement are rejected rather than ignored —
notably, `is uninstall --dry-run` fails instead of quietly uninstalling.

### Keys in the wrapped shell

Ghost text (`--ui ghost`, and the ghost half of the default `hybrid`):

| Key | Action |
|---|---|
| `→` / `End` / `Ctrl-E` | Accept the current ghost suggestion |

Popup (`--ui popup`, and the popup half of `hybrid`):

| Key | Action | Config key |
|---|---|---|
| `↓` | Next suggestion | `bindings.next_suggestion` |
| `↑` | Previous suggestion | `bindings.previous_suggestion` |
| `Tab` | Accept the selected suggestion | `bindings.accept_suggestion` |
| `Esc` | Dismiss until the line changes | `bindings.dismiss_suggestions` |

Everything else is forwarded to the shell unchanged.

## Configuration

`~/.config/inshellisense-rs/rc.toml` (`%APPDATA%\inshellisense-rs\rc.toml` on
Windows). Upstream's `~/.inshellisenserc` and
`~/.config/inshellisense/rc.toml` are also read, for migration. Both camelCase
and snake_case keys are accepted; unknown keys are reported by `is doctor`.

```toml
# Values shown are the defaults.
ui              = "hybrid"   # "ghost" | "popup" | "hybrid"
max_suggestions = 5
use_nerd_font   = false      # Nerd Font glyphs instead of emoji in the popup
use_aliases     = false      # resolve shell aliases before completing

[specs]
path = ["/extra/spec/dir"]

[bindings]
next_suggestion     = { key = "down",   shift = false, control = false }
previous_suggestion = { key = "up",     shift = false, control = false }
accept_suggestion   = { key = "tab",    shift = false, control = false }
dismiss_suggestions = { key = "escape", shift = false, control = false }
```

Environment variables:

| Variable | Effect |
|---|---|
| `INSH_RS_SPECS_DIR` | Extra directory of JSON specs to load |
| `INSH_RS_NO_COREUTILS` | Skip coreutils detection |
| `ISTERM` / `INSH_RS` | Set inside a wrapped session; used as the re-entry guard |

## User-defined specs

Drop TOML files in `~/.config/inshellisense-rs/specs/` to add or override commands.
The schema mirrors the Rust model in [`src/spec/model.rs`](src/spec/model.rs).
Additional JSON spec directories can be loaded with `INSH_RS_SPECS_DIR` or the
`[specs].path` array in `~/.config/inshellisense-rs/rc.toml`.

```toml
# ~/.config/inshellisense-rs/specs/hello.toml
names = ["hello"]
description = "Say hello"

[[subcommands]]
names = ["world"]
description = "Say hello to the world"

[[subcommands]]
names = ["kitty"]
description = "Say hello to a cat"
```

## Coreutils

Microsoft's [Coreutils for Windows](https://github.com/microsoft/coreutils)
ships one multi-call binary with a hardlink per utility, bundling `coreutils`,
`findutils` and a GNU-compatible `grep`. It is a build of
[uutils/coreutils](https://github.com/uutils/coreutils), so the same detection
works for uutils on Linux and macOS.

We do not bundle these tools — we describe whichever one is installed. When a
`coreutils` binary is on `PATH`:

- A `coreutils` spec is registered whose subcommands are whatever
  `coreutils --list` reports, so `coreutils ls --<TAB>` completes `ls`'s
  options. Microsoft's fork drops `timeout`, `kill` and `chmod` — they collide
  with Windows built-ins — and adds `find`, `xargs` and `grep`.
- Utilities the bundled Fig corpus does not cover get a spec synthesized from
  their `--help` — `b2sum`, `numfmt`, the `sha*sum` family and the rest. On a
  uutils 0.9.0 install that is 50 of its 107 utilities; Microsoft's fork ships
  a different set, which is why the list is read from the binary rather than
  hardcoded.
- For a utility the corpus *does* cover, the options the installed binary
  actually accepts are merged in. Fig's `ls` spec is BSD-flavored: it knows
  `-a` but not `--all`, so `ls --all<TAB>` used to complete nothing on a
  coreutils system. Merging keeps Fig's curated descriptions and folds the
  missing long forms into the existing options.

The merge only happens after confirming the command on `PATH` really is that
binary, by comparing `<util> --version` against `coreutils <util> --version`.

Results are cached under `~/.inshellisense/coreutils/`, keyed by the binary's
size and mtime, so the steady-state cost is zero subprocesses.
`INSH_RS_NO_COREUTILS=1` disables detection; `is doctor` reports what it found.

## Architecture

```
┌─────────────────┐   bytes   ┌────────────────┐
│ user's terminal │──────────▶│  platform PTY  │
│                 │           │ (shell wrapper)│
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
                 └────────────│ suggest engine │  ← worker thread
                              │  + spec walker │
                              └────────────────┘
```

The suggestion engine parses the current command-line state using a
tokenizer and resolver ported from inshellisense, walks a registry of
Fig.Spec-shaped specs, and returns a ranked `Vec<Suggestion>`. The
renderer picks the top suggestion and emits grey-foreground ANSI that
positions at the cursor via save/restore.

Suggestions are computed off the event loop. Spec generators shell out
(`git branch`, `docker ps`) with a 5-second default timeout, so running them
inline would freeze keystrokes for as long as the slowest one took.

## Comparison with inshellisense

inshellisense-rs aims for behavioral parity with Microsoft's inshellisense
on the specs it covers. Where it differs:

| | inshellisense | inshellisense-rs |
|---|---|---|
| Runtime | Node + V8 | native Rust binary |
| Shell support | bash, zsh, fish, pwsh, nu, xonsh, cmd | bash, zsh, fish, pwsh, powershell, xonsh, nu, cmd |
| Fig spec coverage | ~715 specs via dynamic import | **1470/1470 loaded (100.0%)** via the static extractor |
| JS runtime for opaque closures | always on (Node) | **none** — pure Rust static extraction only |
| Coreutils / uutils detection | no | yes |
| Ghost-text rendering | yes | yes |
| Right-arrow to accept | yes | yes |

Upstream's own footprint figures are not re-measured here; see
[Footprint](#footprint) for what this port actually costs.

## Roadmap

| Phase | Goal | Status |
|---|---|---|
| 1 | Schema v2 + tokenizer + resolver | ✅ done |
| 2 | `Vec<Suggestion>` output + concurrent multi-generator exec | ✅ done |
| 3 | Extractor tool + spec loader + 715+ spec coverage | ✅ done |
| 4 | `PostProcessKind` DSL + lazy `LoadSpec` + extractor isolation | ✅ done |
| 4.5 | Extractor identifier + property-access resolution | ✅ done |
| 4.6 | Static factory evaluator + template folding → 97.1% coverage | ✅ done |
| 5 | Initial parity corpus + GitHub Actions CI gate | ✅ done |
| 6.1 | `createVersionedSpec` handler → 97.6% | ✅ done |
| 6.2 | `ProjectFile` + `FileExistsThen` generators + shorthand fix → 99.86% pure / 100% loaded | ✅ done |
| 6.6 | Parity expansion to 117 cases + CI threshold tightening | ✅ done |
| 6.7 | Audit remediation for CLI/config/completion/terminal edge cases | ✅ done |
| 6.8 | Full-repo audit: 45 root causes across runtime, extractor, parity scanner, and CI | ✅ done |
| 6.9 | Coreutils / uutils detection and `--help`-derived specs | ✅ done |
| — | Shrink the resident spec index (currently 78 MB of JSON held for lazy lookup) | open |
| ~~6~~ | ~~`rquickjs` JS runtime for opaque closures~~ | dropped — pure Rust path reached 99.86% |

## Credits

inshellisense-rs stands on the shoulders of:

- **[inshellisense](https://github.com/microsoft/inshellisense)** by
  Microsoft — the algorithmic design, the shell integration protocol, the
  parser/resolver reference implementation.
- **[@withfig/autocomplete](https://github.com/withfig/autocomplete)** by
  Hercules Labs (Fig, now AWS) — the ~715 completion specs that make this
  useful on real commands.
- **[bash-preexec](https://github.com/rcaloras/bash-preexec)** by Ryan
  Caloras — the precmd/preexec plumbing for the shell integration.
- **[uutils/coreutils](https://github.com/uutils/coreutils)** and Microsoft's
  [Coreutils for Windows](https://github.com/microsoft/coreutils) — the
  utilities detected and completed at runtime.

All are MIT licensed. See [NOTICE](NOTICE) for full attribution and
[LICENSES/](LICENSES/) for upstream license texts.

**inshellisense-rs is not affiliated with Microsoft, Amazon, Fig, or Hercules Labs.**
"IntelliSense" is a registered trademark of Microsoft Corporation.

## License

MIT — see [LICENSE](LICENSE).
