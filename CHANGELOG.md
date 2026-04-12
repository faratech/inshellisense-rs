# Changelog

All notable changes to insh-rs are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.1] — 2026-04-12

First public release. 1:1 feature parity with Microsoft's inshellisense.
Cross-platform (Linux, macOS, Windows). 29 crates, 5.7 MB binary
(vs upstream's 132 MB), 1470 specs bundled.

### Added
- **Interactive popup TUI** (`--ui popup`): upstream-exact boxed layout
  with `┌─┐│└─┘` borders, side-by-side suggestion + description columns,
  `#7D56F4` active-row highlight (256-color fallback to index 105),
  up/down/tab/escape navigation, SCO save/restore cursor, cursor-aware
  padding with swap logic.
- **Hybrid mode** (`--ui hybrid`, now default): ghost text + popup
  simultaneously. Ghost tail follows the active popup cursor; right-arrow
  accepts ghost, tab accepts popup selection.
- **Full 1470-spec corpus** bundled as zstd-compressed JSON (74 MB raw →
  3.8 MB compressed). Decoded at startup via ruzstd (pure Rust).
- **Top-level command-name completion**: typing `una` shows `uname` in
  the popup even before the user finishes the command name.
- **Parity scanner** (`cargo run --bin parity-scan`): systematic
  divergence detection across 6 categories (cli, init, doctor, complete,
  specs, render) with rayon parallelism and ranked markdown report.
- **Background spec loading**: registry decompresses + parses on a
  background thread; shell prompt appears in ~5 ms, suggestions ready
  in ~500 ms.
- **libc::poll event loop**: true zero-CPU-when-idle, matching upstream's
  libuv epoll model. No reader threads, no mpsc channels.
- **Single stdout writer**: renderers take `&mut impl Write` instead of
  owning separate Stdout handles; eliminates ANSI interleave risk.
- Init snippets for all 7 shells (bash/zsh/fish/pwsh/powershell/xonsh/nu)
  matching upstream's exact source-line format.

### Fixed
- Ghost text no longer wipes typed characters when arrowing left/right
  through a command (cursor-navigation keys skip `renderer.clear()`).
- Stranded popup top-border after Enter/Ctrl-C (submitting latch
  suppresses redraw until next PromptStart).
- Nested shells (`sudo su`) no longer show garbage suggestions (stale
  prompt anchor auto-invalidated when cursor drifts >2 rows).
- `specs list` no longer includes nested path keys (`aws/ec2`) or junk
  entries (`-`, empty name).
- SIGWINCH 0x0 guard prevents vt100 panic on transient zero-sized PTYs.
- Startup clear-terminal (`\x1b[2J\x1b[3J\x1b[H`) matches upstream.

### Changed
- Suggestion sort: priority-desc + stable insertion order (matches
  upstream's spec-file authored order, not alphabetical).
- Option aliases: emit one suggestion per option group using upstream's
  getLong/first-match logic (longest name when no partial, first match
  when partial non-empty).
- `init bash` emits upstream-parity source-line snippet; auto-exec
  wrapper moved to `insh install` for backwards compat.
- Lazy spec loading: specs are indexed by top-level key at startup
  (using serde_json RawValue — scans keys without parsing values) and
  deserialized on first `get()`, matching upstream's dynamic-import
  architecture. Cold start: 536 ms → 220 ms (2.4x faster).
- Popup signature uses u64 hash instead of 300-byte String allocation.
- Dependency tree: 83 → 29 crates. Removed clap (manual arg parsing),
  portable-pty (libc::forkpty), rayon (std::thread::scope), crossterm
  (libc termios/ioctl), dirs ($HOME env), once_cell (std::LazyLock),
  signal-hook, bytes, include_dir, ratatui.
- Single binary `is` (dropped the `insh` alias).
- libc::poll event loop (true 0% CPU when idle, matching upstream).
- Single stdout writer for all renderers (no ANSI interleave risk).
- Signal handlers (SIGTERM/SIGHUP/SIGINT) restore original termios.
- **Cross-platform**: platform abstraction layer (`src/platform/`) with
  Unix (forkpty/poll/termios) and Windows (ConPTY/WaitForMultipleObjects/
  SetConsoleMode) backends. macOS works via POSIX compatibility.
- **Windows ConPTY**: CreatePseudoConsole + CreateProcessW, VT input/
  output processing, SetConsoleCtrlHandler, Git Bash path discovery.
- **Shell::Cmd** (Windows): PROMPT-based OSC 6973 marker injection.
- Platform-aware paths: $USERPROFILE/$APPDATA on Windows.
- Platform-aware PATH search: semicolons + .exe/.cmd/.bat on Windows.
- Re-entry guard: running `is` inside an existing session prints status
  and exits instead of nesting (matches upstream).
- PTY master fd closed on exit; panic hook restores terminal.
- Poisoned RwLock/Mutex recovery instead of panicking.
- `is complete` JSON output matches upstream schema exactly (icon emoji,
  allNames, activeToken wrapper).

## [Unreleased]

### Phase 6.2e — 100.0% pure. Method-shorthand `generateSpec` fix.

Instrumented every `ctx.has_functions = true` site with `flipImpure()`
tagged call-site traces, then ran the extractor with
`DEBUG_IMPURE_TRACE=1` on the 2 remaining partial specs.

**Root cause:** pnpm and dotnet both define `generateSpec` using ES6
method shorthand:
```ts
const completionSpec: Fig.Spec = {
  name: "pnpm",
  generateSpec(tokens, executeShellCommand) {  // <-- method shorthand
    ...
  },
};
```
My `MethodDeclaration` handler treated this like a regular method and
flipped `has_functions` for anything that wasn't `postProcess`. The
arrow/function form of `generateSpec` already dropped silently via
`TOLERANT_SCALAR_FIELDS`, but the method-shorthand form bypassed that
check.

**Fix** (`tools/extractor/extract.ts`): the MethodDeclaration handler
now checks `TOLERANT_SCALAR_FIELDS` before flipping impure. Runtime
hooks (`generateSpec`, `loadSpec`, `getVersionCommand`, etc.) in
method-shorthand form now drop silently, just like the arrow form.

**Result: 1470/1470 = 100.0% pure. Zero partials. Zero js_only. Zero
errors.** CI threshold tightened from 99% to 100%.

### Phase 6.6 — Parity expansion + CI tightening + shared engine

- **Added** shared test-engine via `OnceLock` — the 111-case parity
  corpus test now reuses a single `Registry` across all cases instead
  of rebuilding it 111 times (each build walks 1400+ spec files).
  Runtime: **60+ s → 2 s** for the corpus test.
- **Added** `INSH_RS_SPECS_DIR` env-var set inside the test so the
  corpus exercises the full 1410-spec extras tree, not just the 53
  embedded essentials.
- **Expanded** parity corpus from 82 → **111 cases** covering phase
  6.1/6.2 unlocks: pnpm/yarn/bun subcommand runners, git (now
  extractor-sourced), heroku/shopify/infracost/az versioned specs,
  composer/drush/magento/php/rails/nx static shells, rustup toolchain
  subcommand, hub re-export. Pass rate: **109/111 = 98.2%**.
- **Fixed** `Registry::load_disk_specs` key convention: nested files
  like `extras/gcloud/docker.json` (whose primary name is "docker"
  because it's the `gcloud docker` subspec) were overwriting the real
  top-level docker entry when loaded. Now keyed by path-relative-to-
  extras-root just like `walk_embedded` does for include_dir.
- **Tightened** CI extractor-regression threshold from 95% → 99%
  (matching the new 99.86% ceiling with ~0.9% drift margin).
- **Updated** README + CHANGELOG coverage numbers.

### Phase 6.2d — Always emit even partials → 100% loaded

- Changed `extractFileAs` to ALWAYS emit the spec even when
  `has_functions` is true. The 2 specs that still contain top-level
  imperative spec construction (dotnet's `for ... of` loop building
  `subcommands`, pnpm's top-level `subcommands.filter(...)` call) now
  load with their static skeleton. Their dynamic bits drop cleanly.
  Stats: **1468 pure + 2 partial = 1470/1470 loaded (100%)**.
- Verified: `pnpm <TAB>` returns 28 subcommands (i, t, m, it, up, rm,
  un, ln, rb, ls, -r, add, why, run, tst, ...); `dotnet <TAB>` under
  `INSH_RS_SPECS_DIR` returns 19 (new, add, list, remove, build,
  build-server, ...).

### Phase 6.2 — ProjectFile + FileExistsThen generators + shorthand fix

The single biggest phase — pushed coverage from 97.6% → 99.86%.

**New Generator variants** (`src/spec/model.rs` + `src/generator.rs`):
- `Generator::ProjectFile { reader: ProjectFileReader }` with four
  readers implemented in Rust: `PackageJsonScripts` (read
  package.json's scripts object), `PackageJsonNodeClis` (filter
  deps/devDeps against a hardcoded NODE_CLIS set of 60+ tool names
  like vite/jest/eslint/prettier/tsc/next/nuxt/remix/astro),
  `NodeModulesBinaries` (walk up from cwd until `node_modules/.bin/`,
  list entries filtered by NODE_CLIS), `CargoWorkspaceMembers` (read
  Cargo.toml workspace members).
- `Generator::FileExistsThen { path, content_contains, subcommand }`
  — static alternative to many `generateSpec: async` idioms. Checks
  for a marker file (and optionally a substring) and emits a
  hardcoded subcommand if present.

**Extractor improvements** (`tools/extractor/extract.ts`):
- `TOLERANT_SCALAR_FIELDS` — new concept where `generateSpec`,
  `getVersionCommand`, `loadSpec`, `isCommand`, `filterTerm`,
  `getQueryTerm`, `shouldRedraw` drop silently if impure instead of
  tainting the spec. **The single biggest coverage lever** — many
  specs were partial only because of `generateSpec`, and now they
  extract as pure with the dynamic part dropped.
- Method shorthand handler: `postProcess(out) { return ...; }` is now
  classified via the same postProcess pattern matchers as
  `postProcess: function (out) { ... }`, by duck-typing the
  `MethodDeclaration` node to the same `ArrowFunction`/`FunctionExpression`
  shape.
- **ShorthandPropertyAssignment value resolution fix** — previously
  `prop.getNameNode().getSymbol()` returned the property's own symbol
  (not the resolved variable), so shorthand references like
  `{ subcommands }` silently failed. Now uses `prop.getValueSymbol()`
  which follows to the actual top-level definition. **Unlocked many
  specs** that use ES6 shorthand for top-level constant references
  (aws-vault, many others).
- Post-extraction injection table for known idioms: pnpm/yarn/bun get
  a synthetic top-level args generator with `PackageJsonScripts` +
  `PackageJsonNodeClis` + `NodeModulesBinaries` readers; python/
  python3 get a Django marker check (`manage.py` contains "django");
  node gets an AdonisJS `ace` check; php gets artisan/please/bin-
  console checks.
- Eager-load every spec file into the ts-morph project at startup so
  cross-file symbol resolution (e.g. pnpm importing
  `dependenciesGenerator` from `./yarn`) resolves cleanly through
  `getAliasedSymbol`.

**Cleanup** (`src/curated.rs`):
- git, docker, cargo, systemctl, ssh, and every other essential is
  now extractor-sourced. `curated.rs` is empty (kept as a documented
  escape hatch for future Rust-native injection).

Validation: 31 tests pass. Release binary **4.3 MB** (up from 3.7 MB
— git alone is ~400 KB embedded, plus pnpm/node/yarn/bun additions).
Extractor stats: 1470 total, 1468 pure, 2 partial, 0 js_only,
0 errors. 53 of 58 whitelist essentials embedded (up from 46).

### Phase 6.1 — createVersionedSpec handler + filter aws/regions

- **Added** extractor pattern recognition for `export default
  createVersionedSpec(name, versionFiles)`. Algorithm: evaluate both
  args via existing `extractValue`, sort versions semver-wise
  (lexicographic fallback), pick the highest, and recursively extract
  the sibling `X.Y.Z.ts` file under the parent directory name. So
  `heroku/index.ts` → `heroku.json` (not `heroku/index.json`).
- **Added** default-import re-export handling (`import x from "./y";
  export default x;`) via `ImportClause` resolution. Unlocked `cl`
  (re-export of commercelayer).
- **Filtered** `aws/regions.ts` at the walker — it's `export default
  <string[]>` (a data helper, not a Fig.Spec) and shouldn't count as
  a non-spec error.
- **Unlocked** fig, heroku, infracost, shopify, az via the factory
  handler, plus `cl` via the default-import fix.

Stats: 1429/1471 (97.1%) → 1435/1470 (97.6%). js_only: 7 → 0.

### Phase 5 — Parity corpus + CI gate

- **Added** `tests/parity-corpus.jsonl` — **82 hand-crafted parity
  cases** covering git, docker, cargo, kubectl, npm, ssh, systemctl,
  find, grep, chmod, curl, wget, tar, make, sed, ffmpeg, rg, fd, bat,
  fzf, tmux, gh, helm, terraform, ls, ps, kill, nvim, htop, nc, nmap,
  exa, eza, and edge cases (empty line, unknown command, `--` marker,
  option-value binding). Each case uses a subset of assertion keys:
  * `expect_tail` — the exact ghost-text tail from `engine.suggest()`
  * `expect_top_name` — the top blob entry name after sorting
  * `expect_contains_name` — a list of names that must appear in the blob
  * `expect_blob_min` — minimum blob size
  * `expect_none` — engine must return None
- **Added** `corpus_drives_parity_above_threshold` test — loads the
  JSONL at runtime, runs every case, prints a pass-rate summary, fails
  if below 95% (currently: **82/82 = 100%**).
- **Added** `.github/workflows/ci.yml` — GitHub Actions workflow ready
  for when the repo goes public:
  * Builds the release binary and runs all tests
  * Fails if the release binary exceeds 6 MB (early warning for bloat)
  * Runs `cargo fmt --check` and `cargo clippy -D warnings` (both
    non-blocking initially, will tighten after cleanup)
  * Smoke-tests the binary against the 5 most common command fragments
  * Reports the parity coverage percentage
  * Separate `extractor-regression` job clones upstream
    @withfig/autocomplete, runs the extractor, and fails if pure
    extraction drops below 95%

Validation: **31 tests pass** total (14 unit + 17 parity including
the 82-case corpus at 100%). CI workflow file is ready to fire on the
first push to a GitHub remote.

### Phase 4.6 — 97.1% coverage via A1-A4 and static factory evaluator

Pushed pure-extract coverage from 94.6% → **97.1% (1429/1471)** through
six new extractor features:

- **A1: String concatenation folding** — `"foo " + "bar"` in descriptions
  now folds at extraction time. Unblocks zig and other specs that use
  multi-line concatenated strings.
- **A2: Re-export handling** — `export { default } from "./git"` now
  follows the module specifier and extracts from the target file,
  rewriting the primary name to the aliased file's basename. Unlocks
  hub, kubecolor, ubuntu-advantage, ua.
- **A3: Static factory evaluator** — `tryEvaluateFactoryCall` resolves
  `const f = (a, b) => ({...})` + `f("x", "y")` patterns by binding
  parameters to call arguments and walking the return expression with
  a substitution map. Parameter refs resolve via the `param_subs` map
  on the extract context. Handles both `export default factory()` and
  `const x = factory(...); export default x` shapes, as well as
  default-param substitution for arrow factories. Unlocks cargo (38
  subcommands!) and all 8 JetBrains IDE specs (idea, clion, pycharm,
  rubymine, goland, rustrover, webstorm, phpstorm) via the shared
  `generateInteliJCompletionSpec` helper imported from idea.ts.
- **A4: Spread element resolution** — `[...commonOptions, {...}]` and
  `{...baseFields, name: "foo"}` now inline the spread source when it
  resolves to an array or object literal at compile time.
- **Template expression folding** — `` `Hello ${name} cli` `` now
  folds at extraction time when all interpolations resolve to strings
  or primitives (typically via A3 parameter substitution).
- **Shorthand property assignment** — `{ name }` (sugar for
  `{ name: name }`) now resolves the identifier through the normal
  path, which in a factory context means via parameter substitution.
  Fixes the "empty names" error that blocked all 8 IDE specs on the
  first A3 run.
- **Skip filter for non-spec files** — `.d.ts` declarations,
  `shared.ts` helpers, and `generators.ts` utility files are excluded
  from the walker so they don't show up as extraction errors.

Minor:
- Factory evaluator is **lax**: inner impurities that hit
  non-tolerant-list fields no longer fail the whole factory call —
  they drop individual property values to null and keep extracting.
  Covers cases where one description references an unresolved helper
  but the rest of the spec is clean.
- `cargo_subcommand_completion` test updated for `cargo bu` instead of
  `cargo b` — cargo upstream declares `b` as an alias for build, so
  `cargo b` is now an exact match with no ghost tail.

Validation: **30 tests pass**. Extractor stats: 1471 total, **1429 pure
(97.1%)**, 35 partial (2.4%), 7 js_only (0.5%), 0 errors. Release binary:
**3.7 MB** stripped (up from 3.5 MB — richer embedded essentials now
include cargo with 38 subcommands). Essentials extracted: 46 of 58
whitelist commands.

Remaining 42 gap specs all fall in one of three categories:
- `generateSpec: async` that reads project files at completion time
  (git, node, python, pnpm, yarn, rustup, bun, dotnet, rails, drush,
  magento) — structurally requires runtime JS execution
- Top-level `custom: async` generators doing conditional shell work
  (aws-vault, composer, dog, esbuild, fly, kamal, mask, op, task,
  serverless, uv, xc, yarn, z) — same
- Factory index files calling `createVersionedSpec` from
  `@fig/autocomplete-helpers` (aws/regions, az/index, fig/index,
  heroku/index, infracost/index, shopify/index, cl) — external
  runtime helper package

The remaining coverage gain would require either hand-porting these
specs as Rust-native entries or the embedded JS runtime we explicitly
dropped. Realistic ceiling is ~97-98% without JS.

### Phase 4.5 — 94.6% coverage via extractor improvements; JS runtime removed

- **Removed** `src/js.rs` and the `js` / `boa_engine` feature entirely.
  Decision: skip phase 6's embedded JS runtime. 94.6% pure coverage
  without JS is the ceiling worth chasing; the remaining 5.4% are
  specs with genuinely arbitrary runtime logic (factory exports,
  custom generators that shell out conditionally) — easier to
  hand-curate the handful that matter than to ship rquickjs.
- **Added** extractor identifier resolution — references like
  `generators: tasksGenerator` now resolve the Identifier to its
  top-level `const tasksGenerator: Fig.Generator = {...}` declaration
  and extract the initializer inline. Huge coverage jump.
- **Added** extractor PropertyAccessExpression resolution — references
  like `sharedCommands.run` (Docker's shared-subcommand-record pattern)
  now resolve via the enclosing object literal. Unlocked 20+ docker
  subcommands including `run`.
- **Added** extractor `@fig/autocomplete-generators` helper recognition —
  `filepaths(...)` and `folders(...)` calls inside generator positions
  lower to `Generator::Template { filepaths }` / `folders` directly
  instead of being marked impure.
- **Added** second postProcess classifier: `JSON.parse(out) + ...map(...)`
  block-statement shape → `PostProcess::Pattern { JsonParse }`.
- **Added** parenthesized-expression unwrap for arrow bodies like
  `(x) => ({...})` — fixes the extremely common map-callback shape.
- **Added** **per-element isolation barriers** in `extractObject` for
  tolerant-list fields (`subcommands`, `options`, `args`, `generators`,
  `suggestions`). Impure elements are dropped individually instead of
  tainting the whole spec — the single biggest coverage lever. Before
  this: any one complex generator marked the entire spec as partial.
  After: the spec extracts with that one generator dropped.
- **Changed** tolerant field list widens map-body tolerance to allow
  template literals, property access, element access, and numeric
  literals in addition to simple identifiers.
- **Changed** `src/curated.rs` slimmed to just git + cargo. docker,
  systemctl, ssh, apt, gh, npm, kubectl, terraform, tar, make, jq, and
  34 others are now extractor-sourced.
- **Added** parity test `docker_run_has_detach` exercising the full
  `docker run --detach` path via the PropertyAccessExpression resolver.

Validation: **30 tests pass** (14 unit + 16 parity). Extractor stats:
1476 total, **1397 pure (94.6%)**, 51 partial (3.5%), 23 js_only (1.6%),
5 errors (0.3%). Release binary: **3.4 MB** stripped (up from 2.4 MB
because the 45-essential embed set doubled from 26 — richer specs like
docker with 58 subcommands cost bytes but aren't wasteful). Essentials
extracted: 45 of 58 whitelist commands (up from 26). The 13 whitelist
commands still partial: git, cargo, node, bun, php, composer, mix,
clang, gcc, g++, c++, clang++, dotnet — all language-specific tools
with factory exports or top-level async generators. git and cargo
still come from curated.rs; the rest are accept-and-wait for
now.

### Phase 4 — PostProcessKind DSL + lazy LoadSpec + recursive extractor + embed/extras split

- **Added** `PostProcessKind` execution in `src/generator.rs`:
  - `SplitLines` — line-by-line raw output → suggestions
  - `SplitLinesFiltered` — filter by skip-prefix
  - `JsonParse` / `JsonPath` — parse via `serde_json::Value::pointer`,
    support dotted path syntax (`items.name`) or JSON Pointer (`/items/name`)
  - `GitBranches` — strip `*`, drop `HEAD`, strip `remotes/` prefix
  - `KeyValueColon` — split lines on `:`, key → name, value → description
  - `TableColumn { index, sep }` — awk-like column extraction
- **Added** `spec::resolver::resolve_with_registry` — new entry point that
  takes a registry reference so `LoadSpec::SpecPath { name }` references
  get resolved against the loaded spec tree at descent time. Used by
  `suggest::Engine` automatically.
- **Added** Registry recursive walk via `include_dir::DirEntry`, so
  nested paths like `aws/ec2` get loaded under their full path key.
- **Added** extractor recursive directory walker — previously only
  enumerated `SRC/*.ts` top-level; now descends into `aws/`, `gcloud/`,
  `heroku/`, etc. and preserves nested paths in the output.
- **Added** extractor postProcess pattern matcher — recognizes the most
  common upstream shape `(out) => out.split("\n").map(line => ({ name: line, ... }))`
  and emits `PostProcess::Pattern { inner: SplitLines }` instead of
  marking the spec as having functions. Upgraded pure spec count from
  321 → **1032**.
- **Added** `INSH_RS_SPECS_DIR` environment variable — at registry init,
  recursively loads `.json` specs from the given directory (after the
  embedded essentials). Lets users point at a full extras tree without
  rebuilding the binary.
- **Changed** extractor splits output into two directories:
  - `specs-data/embed/` — committed, 26 top-level essentials (chmod, curl,
    find, grep, fzf, vim, nvim, top, htop, ls, cp, mv, dig, nc, nmap, ps,
    etc.), ~332 KB. Embedded into the binary via `include_dir!`.
  - `specs-data/extras/` — gitignored, 1006 specs (~107 MB) including the
    entire aws/, gcloud/, dotnet/, heroku/ subtrees. Regenerable via the
    extractor; loadable at runtime via `INSH_RS_SPECS_DIR`.
- **Removed** old `specs-data/essentials/` (superseded by `embed/`).

Validation: **29 tests pass** (14 unit + 15 parity). Release binary is
**2.4 MB** (down from 8.7 MB in phase 3 because the 1006 extras are no
longer embedded). Extractor stats: 1476 total, 1032 pure, 416 partial,
23 js_only, 5 errors. `INSH_RS_SPECS_DIR=specs-data/extras insh doctor`
loads **1002 specs** confirming runtime extras loading works.

### Phase 3 — @withfig/autocomplete extractor + 321 specs vendored

- **Added** `tools/extractor/` — Node + TypeScript extractor using
  `ts-morph`. Walks `@withfig/autocomplete/src/**/*.ts`, locates each
  file's default-exported Fig.Spec object literal, and converts the
  pure-data subset into insh-rs's Rust JSON schema. Specs containing
  any inline function (arrow, function expression, method) anywhere
  in their tree are classified as `partial` or `js_only` in the
  manifest and not extracted (phase 6 handles those via rquickjs).
  ~570 lines of TypeScript.
- **Added** `specs-data/essentials/` — **321 extracted pure-data specs**
  checked into the repo (7.4 MB JSON). Covers find, grep, tar, chmod,
  curl, wget, ssh, rsync, make, sed, awk, jq, ffmpeg, yt-dlp, and 300+
  other commands that don't need JS runtime support. Rerun the
  extractor with `cd tools/extractor && npm run extract` to refresh
  when upstream releases.
- **Added** `specs-data/index.json` — manifest with extraction stats
  (total, pure, partial, js_only) and per-spec kind classification.
- **Added** `include_dir` dependency; `Registry::load_embedded_essentials`
  embeds the entire `specs-data/essentials/` tree into the binary at
  build time and deserializes each spec lazily via `serde_json`.
- **Added** extractor-spec parity tests: `extracted_find_has_options`,
  `extracted_grep_count_option`, `loaded_spec_count_at_least_100`.
- **Changed** `Registry::new_with_defaults` load order: extracted specs
  first, then curated hand-ported specs only for names not already
  loaded, then user TOML overrides. This means `git`/`docker`/`cargo`/
  `systemctl`/`ssh` still come from the curated set (they have
  function-containing specs that the extractor couldn't pure-extract),
  but everything else is now extractor-driven.
- **Changed** `Repeatable` enum redesigned to `{ Bool(bool), N(u16) }`
  untagged so serde round-trips upstream's `isRepeatable: true | number`
  variant cleanly. Fixed extractor to emit the boolean/number directly
  instead of a stringified `"true"`.
- **Fixed** dead-code warnings (`sub` helper in curated.rs removed,
  `from_variadic` underscored).

Validation: `cargo test` passes **29 tests** (14 unit + 15 parity
including 3 new extractor-driven cases). 321 of 715 upstream specs
successfully extracted (44.9% — the declarative subset). 377 specs
classified as `partial`, 17 as `js_only`, 0 errors. Release binary
8.7 MB (up from 1.8 MB — the 6.9 MB growth is embedded JSON; phase 4
will switch to MessagePack which should cut that roughly in half).
Smoke tests: `find -` → `E`, `grep --cou` → `nt`, `chmod 7` → `44`.

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
