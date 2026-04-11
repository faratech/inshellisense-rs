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
        use include_dir::{include_dir, Dir};
        static ESSENTIALS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/specs-data/essentials");
        for file in ESSENTIALS.files() {
            if file.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(bytes) = file.contents_utf8() else { continue };
            match serde_json::from_str::<Subcommand>(bytes) {
                Ok(spec) => self.insert(spec),
                Err(e) => {
                    // A field we don't yet model — log and skip so the rest
                    // of the registry still loads.
                    eprintln!(
                        "insh-rs: skipped {}: {}",
                        file.path().display(),
                        e
                    );
                }
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
                Err(e) => eprintln!("insh-rs: failed to load {}: {}", path.display(), e),
            }
        }
    }
}
