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
        static EMBED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/specs-data/embed");
        Self::walk_embedded(&EMBED, self);
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
                    eprintln!("insh-rs: failed to parse {}", path.display());
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

    fn walk_embedded(dir: &include_dir::Dir<'_>, reg: &mut Registry) {
        for entry in dir.entries() {
            match entry {
                include_dir::DirEntry::Dir(d) => Self::walk_embedded(d, reg),
                include_dir::DirEntry::File(f) => {
                    if f.path().extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    let Some(bytes) = f.contents_utf8() else { continue };
                    // Derive the registry key from the path relative to the
                    // essentials root: `aws/ec2.json` → `aws/ec2`.
                    let rel_path = f.path();
                    let rel_key = rel_path
                        .with_extension("")
                        .to_string_lossy()
                        .to_string();
                    match serde_json::from_str::<Subcommand>(bytes) {
                        Ok(mut spec) => {
                            // For nested paths, the primary name comes from
                            // the spec itself but we also want it reachable
                            // under the path key for LoadSpec::SpecPath.
                            if rel_key.contains('/') {
                                reg.specs.insert(rel_key, spec);
                            } else {
                                // Normal top-level spec — keyed by its own name.
                                // Strip the redundant primary-name check; `insert`
                                // already does it.
                                let _ = rel_key;
                                reg.insert(std::mem::take(&mut spec));
                            }
                        }
                        Err(e) => {
                            eprintln!("insh-rs: skipped {}: {}", rel_path.display(), e);
                        }
                    }
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
