# Remediation plan — August 2026 audit

> **Status: COMPLETE (2026-08-22).** All 37 findings were fixed and shipped
> in v0.0.5 the same day. This document is retained as the record of the
> workstreams and their rationale; per-issue detail lives in #48–#84.

Findings from the 2026-08-22 multi-agent bug hunt (8 dimension reviewers over
disjoint file slices → cross-dimension dedup → adversarial verification per
slice → completeness critic; 37 raw findings, 34 confirmed by independent
verification, 3 more verified inline after the hunt, 1 refuted and not filed).
Every finding is filed as a GitHub issue (#48–#84). Prior audit: #2–#46.

## Refuted during verification (not filed)

- `src/render/popup.rs:236` — popup direction/fit check. Verifier showed the
  renderer clamps to the available rows before drawing; scenario cannot occur.

## Workstreams

Issues are grouped into nine workstreams. Within a workstream the fixes share
context and review, so they should land as one batch (one branch, one PR,
tests included) unless noted.

### WS-1 — Tracked-command-line integrity (P1 cluster, do first)

The text `is` believes is on the line is corrupted by five independent bugs
that compound: wide chars (#55), the `=` tokenizer split (#53), a UTF-8 panic
in option resolution (#54), OSC tail truncation (#56), and ghost-tail
whitespace splitting (#67). Fix #54 first — it is a reachable panic — then
#53/#55 (tokenizer + tracker), then #56/#67 (downstream consumers).

- #54 (P1, panic, S) `resolver.rs:466` — byte-offset slice inside a multibyte
  char. Fix: operate on char boundaries (`char_indices` / `is_char_boundary`).
- #53 (P1, S) `parser.rs:152` — `=` splits any word. Fix: split only while
  `reading_flag`. NOTE: the unguarded `=` mirrors upstream `parser.ts:100`;
  this is a deliberate divergence from upstream, worth a code comment.
- #55 (P1, M) `term.rs:307` — wide-char continuation cells become spaces.
  Fix: skip continuation cells (cell has no `chars` of its own) in `row_text`.
- #56 (P2, M) `ansi.rs:132` — withheld OSC tail beyond `MAX_PENDING` is
  silently dropped, losing prompt markers on slow streams. Fix: grow the
  buffer or flush-and-resync with a warning instead of discarding.
- #67 (P2, M) `suggest.rs:616` — ghost tail splits on raw whitespace,
  ignoring the tokenizer's quote/escape handling. Fix: reuse the tokenizer's
  last-token span instead of `split_whitespace`.

Regression tests: CJK command line, `env FOO=bar git ch` (repro:
`is complete "env FOO=bar git ch"` → currently 0 suggestions), multibyte
option value (`git -m "héllo" --<TAB>`), long-running output followed by
prompt markers split across reads.

### WS-2 — PTY event-loop lifecycle (P1 hang + P2 spin)

- #52 (P1, M) `pty.rs:228` — post-exit drain blocks on the master fd until
  every grandchild closes the slave; `sleep 300 &` + `exit` freezes the
  terminal (reproduced: teardown tracks the background job's lifetime; raw
  mode stays on and keystrokes are dropped). Fix: `poll(0)`-gated drain or
  `O_NONBLOCK` on the master, mirroring the main loop's readiness checks.
- #61 (P2, S) `unix.rs:122` — stdin EOF/invalid fd busy-spins at ~100% CPU
  (reproduced with `< /dev/null`). Fix: drop stdin from the poll set on
  EOF/POLLERR/POLLNVAL; also make `enable_raw_mode` failure fatal (the
  tool cannot work without a tty) instead of the current silent no-op.

Regression tests: spawn `bash -c 'sleep N & exit'` through the PTY harness —
teardown must complete in <1 s regardless of N; run with stdin at EOF —
process must not burn CPU and must exit or error clearly.

### WS-3 — Windows input & console hardening

Windows-only; cannot be regression-tested in CI here. Land behind manual
field testing per CLAUDE.md (PowerShell + Git Bash in Windows Terminal).

- #50 (P1, M) `windows.rs:174` — stdin assumed to be a console buffer;
  piped/file stdin silently loses all keys or freezes. Fix: detect handle
  type; refuse or fall back to a line-mode reader.
- #51 (P1, M) `windows.rs:339` — the vk==0 filter (added by the #24 fix to
  drop synthetic ENABLE_VIRTUAL_TERMINAL_INPUT events) also discards pasted
  text, IME-composed text, and unmapped characters. Fix: filter synthetic
  events by their actual signature (repeating scancode block / message-based)
  rather than by vk==0, so reportText-style input survives.
- #62 (P2, S) `windows.rs:360` — Alt-modified printables dropped or forwarded
  without the ESC prefix. Fix: emit `ESC <char>` for Alt+printable.
- #75 (P3, S) `windows.rs:82` — `DeleteProcThreadAttributeList` never called.
  Fix: drop guard in the spawn path.
- #76 (P3, S) `windows.rs:133` — `env::vars()` panics on non-Unicode env.
  Fix: `env::vars_os` with lossy conversion where the value is display-only.
- #79 (P3, S) `shell.rs:140` — `-Login` is not a PowerShell 5.1 parameter.
  Fix: gate on pwsh (7+) only.
- #63 (P2, M) `pty.rs:830` — File/Folder acceptance quotes with POSIX rules
  on every shell (supersedes the quoting gap left after #21). Fix: pick the
  quoting strategy from the active `Shell` (cmd: double quotes; PowerShell/
  nushell: `'…'` with `''` doubling; POSIX: current `'\''` splice).

### WS-4 — Suggestion data sources

- #48 (P1, S) `history.rs:28` — one non-UTF-8 byte silently discards the
  whole history. Fix: `from_utf8_lossy` / line-wise lossy decode.
- #68 (P3, S) `alias.rs:48` — parses the full stdout of `bash -i -c alias`
  as alias definitions; rc-file noise becomes fake aliases. Fix: parse only
  lines matching alias syntax, or use a marker-delimited payload.
- #72 (P3, S) `generator.rs:385` — history-template reads the auto-detected
  parent shell's history file, not the wrapped shell's. Fix: thread the
  active `Shell` through the template.
- #83 (P3, S) `suggest.rs:129` — first-word ordering nondeterministic
  (HashMap iteration). Fix: sort candidates by name before ranking.
- #70 (P3, S) `coreutils.rs:229` — a transient probe timeout is cached
  forever as owns()=false. Fix: don't persist timeout outcomes.

### WS-5 — Spec-loading fidelity

- #80 (P3, S) `spec/model.rs` — no camelCase aliases, so raw Fig JSON parses
  "successfully" with every flag defaulted. Fix: `#[serde(alias)]` on the
  flagged fields (or `rename_all = "camelCase"` + snake_case aliases).
- #81 (P3, S) `spec/parser.rs:94` — whitespace escape hardcoded to `\`;
  PowerShell uses `` ` ``, cmd uses `^`. Fix: per-shell escape char.
- #82 (P3, S) `spec/resolver.rs:515` — after a bare `--`, options are
  suppressed but subcommands are still offered (residue of #23). Fix: treat
  everything after `--` as positional-only.
- #66 (P2, S) `spec/mod.rs:237` — `walk_disk` joins nested spec keys with
  the platform separator; on Windows nested disk specs register as bogus
  top-level commands. Fix: always use `/` in registry keys.
- #57 (P2, M) `coreutils.rs:400` — `parse_help` treats any indented
  `-`-leading line as an option, manufacturing bogus options into cached
  specs. Fix: require option-line shape (flag + space + description) and
  drop usage/epilog sections.

### WS-6 — Install / init robustness

- #64 (P2, M) `resources.rs:145` — resource tree stamped "current" without
  verifying written content; a truncated integration script is never repaired
  and `doctor` calls it healthy. Fix: hash-compare on stamp (or verify after
  write), and make doctor re-check content, not just the stamp.
- #65 (P2, S) `shell.rs:87` — fish `--init-command` receives an unquoted
  path; breaks on spaces (and all of Windows). Fix: quote the path.
- #78 (P3, S) `resources.rs:249` — generated rc blocks use `exec is start`
  (or `is start` + `exit`), so any `is` startup failure kills the user's
  shell/terminal. Fix: fall back to plain shell exec when `is` is missing or
  fails (`is start … || exec <shell>`).
- #71 (P3, S) `doctor.rs:30` — hardcoded ANSI colors in piped output. Fix:
  TTY detection + `NO_COLOR` support.

### WS-7 — Coreutils integration hygiene

- #60 (P2, S) `platform/mod.rs:114` — `find_on_path` accepts any existing
  path (no `is_file()`, no execute bit) and resolves empty PATH elements
  against CWD; shadows both shell spawn and coreutils detection
  (`coreutils.rs:64`/`:212` downstream). Fix: require a regular file with an
  execute bit; skip empty PATH entries.
- #69 (P3, S) `coreutils.rs:98` — cache files written non-atomically; a torn
  file is trusted forever. Fix: write temp + rename.
- #70 — see WS-4.

### WS-8 — Parity/dev-tooling truthfulness

The scanner's failure mode must be "fail loudly", never "vacuous pass".

- #49 (P1, S) `parity/complete.rs:223` — parses our JSON as a flat array but
  the binary emits the `{"suggestions":[…]}` wrapper; every comparison fails
  (or mis-parses). Fix: parse the real schema.
- #59 (P2, S) `parity/specs.rs:204` — unparseable upstream output collapses
  to an empty set, so the specs category passes vacuously. Fix: treat
  unparsable output as failure.
- #58 (P2, S) `parity/mod.rs:224` — scanner children still inherit
  spec-source inputs (`INSH_RS_SPECS_DIR`, operator config, coreutils probe)
  → false divergences (narrower residue of #42). Fix: strip the env vars.
- #74 (P3, S) `parity/render.rs:167` — forkpty failure recorded as an empty
  capture = perfect parity. Fix: propagate the error as failure.
- #84 (P3, S) `tests/cli.rs:36` — completion tests inherit developer HOME /
  spec-dir env. Fix: scrub env in the test harness.

### WS-9 — Panic-safety sweep

- #73 (P3, S) `main.rs:7` — `env::args()` panics (exit 101) on non-UTF-8
  argv; `is complete` crashes on lines with raw bytes. Fix: `args_os` +
  lossy decode at the boundary.
- #76 — see WS-3.
- #54 — see WS-1.

### WS-10 — Renderer redraw correctness

- #77 (P3, S) `render/popup.rs:224` — `draw_full`'s signature hash omits
  `swap_description` and `term_cols`, so a needed popup redraw is skipped,
  leaving a stale or mispositioned popup after a description swap/resize.
  Fix: include both fields in the signature.

## Suggested execution order

1. **WS-1** (#54 → #53 → #55 → #56 → #67) — user-visible correctness of every
   suggestion; includes the only reachable panic outside argv/env.
2. **WS-2** (#52, #61) — hang and CPU spin are the worst operational failures.
3. **WS-8** (#49 first) — until the scanner tells the truth, parity results
   can't validate anything else.
4. **WS-4 + WS-7 + WS-9** — small, independent, low-risk batch (all S-sized).
5. **WS-5 + WS-6** — spec fidelity then install robustness.
6. **WS-10 + WS-3 last** — renderer tweak rides anywhere; Windows-only items
   batch the manual field test once.

Sizes: S ≈ under an hour, M ≈ a few hours incl. tests. Nothing here is L;
the largest single item (#51) is contained in `read_stdin`.

## Validation gates per PR

- `cargo fmt --check && cargo clippy --release --all-targets -- -D warnings`
- `cargo test --all-targets` plus the new regression tests listed per WS
- Repro commands from the issue bodies (each P1 has one that fails today):
  - #53/#55: `is complete "env FOO=bar git ch"` / CJK line tracking
  - #54: multibyte option value through `find_separated_option`
  - #52: `sleep N & exit` teardown time via the PTY harness
  - #61: `< /dev/null` CPU burn check
  - #48: history file containing one invalid byte
  - #49: any complete-category corpus run before/after
- Windows items (#50, #51, #62, #63, #75, #76, #79): manual pass in Windows
  Terminal (PowerShell 7, PowerShell 5.1 for #79, cmd for #63) before merge.

## Dependency/toolchain refresh (same day, no issues)

MSRV 1.85 → 1.88, ruzstd 0.8 → 0.9, Cargo.lock refreshed (19 packages,
syn → 3.x transitively), 26 collapsible-if sites updated for rustc 1.98
clippy. CI unchanged (`dtolnay/rust-toolchain@stable`).
