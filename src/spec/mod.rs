//! Fig spec module — v2 schema + tokenizer + resolver.
//!
//! Public surface: the Registry (loads curated + TOML user specs), the
//! schema types (Subcommand, Opt, Arg, Generator, etc.), the tokenizer
//! (parse_command → Vec<CommandToken>), and the resolver (walks the spec
//! tree). The suggestion engine in `src/suggest.rs` is the main consumer.

pub mod filter;
pub mod model;
pub mod parser;
pub mod resolver;

pub use model::{
    Arg, CacheSpec, CacheStrategy, FilterStrategy, Generator, LoadSpec, Opt, ParserDirectives,
    PostProcess, PostProcessKind, Repeatable, ScriptInput, Spec, Subcommand, Suggestion,
    SuggestionType, Template,
};
pub use parser::{parse_command, CommandToken};
pub use resolver::{resolve, ResolveResult};

use std::collections::BTreeMap;

#[derive(Default)]
pub struct Registry {
    specs: BTreeMap<String, Subcommand>,
}

impl Registry {
    pub fn new_with_defaults() -> Self {
        let mut r = Self::default();
        // Extractor-produced specs take priority — they cover ~45% of the
        // @withfig/autocomplete library.
        r.load_embedded_essentials();
        // Curated hand-ported specs fill gaps that the extractor couldn't
        // handle (anything with inline functions — phase 6 closes this).
        for spec in crate::curated::all() {
            // Only insert if not already loaded from essentials.
            let name = spec.name().to_string();
            if !r.specs.contains_key(&name) {
                r.insert(spec);
            }
        }
        r.load_toml_dir();
        r
    }

    fn load_embedded_essentials(&mut self) {
        // The full 1470-spec corpus (extractor output + curated
        // essentials, 78MB raw JSON) is bundled into one JSON object
        // at build time and zstd-compressed to ~3.8MB. We decode it
        // on first launch via ruzstd (pure Rust) and parse it into
        // the registry in a single pass — deserializing directly
        // into `BTreeMap<String, Subcommand>` instead of going
        // through `serde_json::Value` as an intermediate. That
        // single change shaves ~400ms off cold startup by avoiding
        // the full double-parse (JSON → Value → Subcommand).
        const BUNDLE_ZST: &[u8] =
            include_bytes!("../../specs-data/bundle.json.zst");
        use std::io::Read;
        let mut decoder = match ruzstd::decoding::StreamingDecoder::new(BUNDLE_ZST) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("is: failed to start zstd decoder: {}", e);
                return;
            }
        };
        let mut decoded = Vec::with_capacity(80 * 1024 * 1024);
        if let Err(e) = decoder.read_to_end(&mut decoded) {
            eprintln!("is: failed to decode spec bundle: {}", e);
            return;
        }
        // Parse directly into the target type. `from_slice` skips the
        // UTF-8 validation step that `from_str` does — safe because
        // zstd's output is known valid UTF-8 from the JSON we
        // compressed at build time.
        let map: BTreeMap<String, Subcommand> =
            match serde_json::from_slice(&decoded) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("is: failed to parse spec bundle: {}", e);
                    return;
                }
            };
        for (key, spec) in map {
            if key.contains('/') {
                self.specs.insert(key, spec);
            } else {
                self.insert(spec);
            }
        }
        // Also load runtime extras if INSH_RS_SPECS_DIR is set.
        if let Ok(extras) = std::env::var("INSH_RS_SPECS_DIR") {
            self.load_disk_specs(std::path::Path::new(&extras));
        }
    }

    fn load_disk_specs(&mut self, root: &std::path::Path) {
        Self::walk_disk(root, root, self);
    }

    fn walk_disk(root: &std::path::Path, dir: &std::path::Path, reg: &mut Registry) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_disk(root, &path, reg);
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let Ok(bytes) = std::fs::read_to_string(&path) else { continue };
                let Ok(spec) = serde_json::from_str::<Subcommand>(&bytes) else {
                    eprintln!("is: failed to parse {}", path.display());
                    continue;
                };
                // Derive the registry key from the path relative to the
                // extras root: `gcloud/docker.json` → `gcloud/docker`.
                // Top-level files key by primary name so that
                // `git.json` keys as "git" (not "git" path-key).
                if let Ok(rel) = path.strip_prefix(root) {
                    let key = rel.with_extension("").to_string_lossy().into_owned();
                    if key.contains('/') {
                        // Don't overwrite an existing top-level spec
                        // with a nested subspec that happens to share
                        // a primary name (e.g. gcloud/docker.json).
                        reg.specs.insert(key, spec);
                        continue;
                    }
                }
                reg.insert(spec);
            }
        }
    }

    pub fn insert(&mut self, spec: Subcommand) {
        // Register under every alias so `git co` matches a spec whose
        // primary name is `git` but is keyed by the first alias.
        let primary = spec.name().to_string();
        // Only the primary name goes into the top-level lookup; aliases are
        // resolved via subcommand.matches() during resolution.
        self.specs.insert(primary, spec);
    }

    pub fn get(&self, name: &str) -> Option<&Subcommand> {
        self.specs.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.specs.keys().map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.specs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    fn load_toml_dir(&mut self) {
        let Some(base) = dirs::config_dir() else {
            return;
        };
        let dir = base.join("insh-rs").join("specs");
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            match toml::from_str::<Subcommand>(&src) {
                Ok(spec) => self.insert(spec),
                Err(e) => eprintln!("is: failed to load {}: {}", path.display(), e),
            }
        }
    }
}
