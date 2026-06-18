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
pub use parser::{CommandToken, parse_command};
pub use resolver::{ResolveResult, resolve};

use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Lazy-loading spec registry. Matches upstream inshellisense's
/// architecture: at startup we decompress the zstd bundle and build a
/// lightweight name→byte-offset index (just scanning top-level JSON
/// keys, no deep parsing). Individual specs are deserialized on first
/// `get()` and cached. This cuts startup from ~500ms (parse all 1470
/// specs) to ~150ms (decompress + index scan), with ~1ms per-spec
/// cost on first lookup.
pub struct Registry {
    /// Eagerly parsed specs (curated, TOML user specs, and cached
    /// lazy-loaded specs from the bundle). Behind UnsafeCell so
    /// `get(&self)` can insert lazily-parsed specs. Safety: the
    /// Registry is always behind Arc<RwLock> in the Engine, so
    /// the RwLock guarantees single-threaded access at the point
    /// of mutation.
    specs: UnsafeCell<BTreeMap<String, Subcommand>>,
    /// Lazy index: key → raw JSON bytes for specs not yet parsed.
    lazy: Mutex<BTreeMap<String, Vec<u8>>>,
}

// Safety: Registry is behind Arc<RwLock<Option<Engine>>> in pty.rs.
// The RwLock ensures mutual exclusion. UnsafeCell is only accessed
// from `get()` which runs on the main thread while holding the lock.
unsafe impl Sync for Registry {}
unsafe impl Send for Registry {}

impl Default for Registry {
    fn default() -> Self {
        Self {
            specs: UnsafeCell::new(BTreeMap::new()),
            lazy: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Registry {
    pub fn new_with_defaults() -> Self {
        let mut r = Self::default();
        r.load_embedded_lazy();
        // Curated hand-ported specs override the bundle.
        for spec in crate::curated::all() {
            let name = spec.name().to_string();
            r.lazy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&name);
            if !r.specs.get_mut().contains_key(&name) {
                r.insert(spec);
            }
        }
        r.load_configured_json_dirs();
        r.load_toml_dir();
        r
    }

    fn load_embedded_lazy(&mut self) {
        // Decompress the zstd bundle into raw JSON bytes, then scan
        // the top-level object keys to build a name→bytes index
        // WITHOUT parsing any Subcommand values. This is the lazy
        // equivalent of upstream's `loadSpecsSet()` which maps
        // command names to file paths without loading the files.
        const BUNDLE_ZST: &[u8] = include_bytes!("../../specs-data/bundle.json.zst");
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

        // Parse the top-level JSON object as a map of RawValue —
        // this scans keys but does NOT deserialize the nested spec
        // objects. Each value is kept as raw JSON bytes for lazy
        // deserialization on first get().
        // Parse the top-level JSON object as a map of RawValue —
        // this scans keys but does NOT deserialize the nested spec
        // objects. Each value is kept as raw JSON bytes for lazy
        // deserialization on first get().
        let map: BTreeMap<String, Box<serde_json::value::RawValue>> =
            match serde_json::from_slice(&decoded) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("is: failed to index spec bundle: {}", e);
                    return;
                }
            };
        {
            let mut lazy = self.lazy.lock().unwrap_or_else(|e| e.into_inner());
            for (key, raw) in map {
                lazy.insert(key, raw.get().as_bytes().to_vec());
            }
        } // drop the lock before calling load_disk_specs
    }

    fn load_configured_json_dirs(&mut self) {
        let mut dirs = std::collections::BTreeSet::<String>::new();
        if let Ok(extras) = std::env::var("INSH_RS_SPECS_DIR") {
            dirs.insert(extras);
        }
        for dir in crate::config::load().specs.path {
            dirs.insert(dir);
        }
        for dir in dirs {
            self.load_disk_specs(std::path::Path::new(&dir));
        }
    }

    fn load_disk_specs(&mut self, root: &std::path::Path) {
        Self::walk_disk(root, root, self);
    }

    fn walk_disk(root: &std::path::Path, dir: &std::path::Path, reg: &mut Registry) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_disk(root, &path, reg);
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let Ok(bytes) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(spec) = serde_json::from_str::<Subcommand>(&bytes) else {
                    eprintln!("is: failed to parse {}", path.display());
                    continue;
                };
                if let Ok(rel) = path.strip_prefix(root) {
                    let key = rel.with_extension("").to_string_lossy().into_owned();
                    if key.contains('/') {
                        reg.specs.get_mut().insert(key, spec);
                        continue;
                    }
                }
                reg.insert(spec);
            }
        }
    }

    pub fn insert(&mut self, spec: Subcommand) {
        let primary = spec.name().to_string();
        self.specs.get_mut().insert(primary, spec);
    }

    /// Look up a spec by command name. If the spec hasn't been parsed
    /// yet (still in the lazy index), parse it now and cache it.
    /// Uses `&self` so callers don't need mutable access — the lazy
    /// map is behind a Mutex for interior mutability.
    pub fn get(&self, name: &str) -> Option<&Subcommand> {
        // Safety: single-threaded access guaranteed by Arc<RwLock> in
        // the Engine. We only insert into specs (never remove), so
        // existing references remain valid after insertion.
        let specs = unsafe { &*self.specs.get() };
        if let Some(s) = specs.get(name) {
            return Some(s);
        }
        let raw = {
            let mut lazy = self.lazy.lock().unwrap_or_else(|e| e.into_inner());
            lazy.remove(name)
        };
        if let Some(raw) = raw {
            match serde_json::from_slice::<Subcommand>(&raw) {
                Ok(mut spec) => {
                    // Surgically augment bundled specs to close extractor gaps
                    // (missing options, opaque-JS generators, lost fields).
                    crate::curated::patch(&mut spec, name);
                    let specs = unsafe { &mut *self.specs.get() };
                    specs.insert(name.to_string(), spec);
                    return unsafe { &*self.specs.get() }.get(name);
                }
                Err(e) => {
                    eprintln!("is: failed to parse spec `{}`: {}", name, e);
                }
            }
        }
        None
    }

    /// Returns all registered names (both parsed and lazy).
    pub fn names(&self) -> Vec<String> {
        let lazy = self.lazy.lock().unwrap_or_else(|e| e.into_inner());
        let specs = unsafe { &*self.specs.get() };
        let mut names: Vec<String> = specs.keys().cloned().collect();
        names.extend(lazy.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    pub fn len(&self) -> usize {
        let lazy = self.lazy.lock().unwrap_or_else(|e| e.into_inner());
        let specs = unsafe { &*self.specs.get() };
        specs.len() + lazy.len()
    }

    pub fn is_empty(&self) -> bool {
        let lazy = self.lazy.lock().unwrap_or_else(|e| e.into_inner());
        let specs = unsafe { &*self.specs.get() };
        specs.is_empty() && lazy.is_empty()
    }

    fn load_toml_dir(&mut self) {
        let Some(dir) = crate::paths::user_specs_dir() else {
            return;
        };
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
