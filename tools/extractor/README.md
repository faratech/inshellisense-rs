# inshellisense-rs extractor

Converts `@withfig/autocomplete` TypeScript specs into JSON that inshellisense-rs
loads at runtime. Produces one `.json` per extracted spec plus an
`index.json` manifest.

## Install

```sh
npm install
```

## Run

```sh
SRC=/path/to/withfig-autocomplete/src \
OUT=../../specs-data \
npm run extract
```

Defaults: `SRC=/tmp/withfig-autocomplete/src` and
`OUT=../../specs-data` (relative to this directory).

## What it does

For each `.ts` file in `SRC`, ts-morph parses the default export and
walks the Fig.Spec object literal. Pure object/array/primitive trees
are converted to inshellisense-rs's Rust schema and written to
`OUT/essentials/<name>.json`. Specs that contain any inline function
(arrow/function/method anywhere in their tree) are classified as
`partial` or `js_only` in the manifest and **not** extracted to JSON.
Coverage reached 100% via static extraction; no JS runtime needed.

## Classification

| Kind | Condition |
|---|---|
| `pure` | No functions anywhere; fully converted to JSON |
| `partial` | Object literal exists but contains functions; JSON not emitted |
| `js_only` | No object literal at the default export (factory function, etc.) |

See `stats` in the manifest for counts per run.

## Schema mapping

| Upstream Fig field | Rust field | Notes |
|---|---|---|
| `name: string \| string[]` | `names: Vec<String>` | always stored as array |
| `displayName` | `display_name` | |
| `priority` | `priority` | clamped to 0..100 |
| `isPersistent` | `is_persistent` | option-level |
| `isRepeatable: true \| number` | `is_repeatable: bool \| u16` | untagged enum |
| `exclusiveOn` / `dependsOn` | `exclusive_on` / `depends_on` | |
| `parserDirectives.*` | `parser_directives.*` | |
| `args: T \| T[]` | `args: Vec<T>` | always stored as array |
| `generators: T \| T[]` | `generators: Vec<T>` | always stored as array |
| `template: T \| T[]` | `templates: Vec<T>` | always stored as array |
| `loadSpec: string` | `load_spec: { kind: "spec_path", name }` | function form = phase 6 |

## License

MIT. See the top-level [LICENSE](../../LICENSE) and
[NOTICE](../../NOTICE) for attribution to @withfig/autocomplete upstream.
