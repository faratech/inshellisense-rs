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

use std::collections::BTreeMap;
use std::sync::Mutex;

/// `get(&self)` hands out `&Subcommand` borrowed from inside the map while
/// only holding a shared borrow of the Registry, so `Subcommand` must be
/// shareable across threads for `Registry: Sync` to be sound.
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assertions() {
        assert_send_sync::<Subcommand>();
    }
    let _ = assertions;
};

#[derive(Default)]
struct Inner {
    /// Parsed specs (curated, TOML user specs, and lazily-parsed bundle
    /// specs). Values are boxed so their addresses stay put while the map
    /// rebalances — `get(&self)` returns references into these allocations.
    specs: BTreeMap<String, Box<Subcommand>>,
    /// Lazy index: key → raw JSON bytes for specs not yet parsed.
    lazy: BTreeMap<String, Vec<u8>>,
    /// Root alias → primary key in `lazy`. A spec's root `names` may list
    /// aliases (`["R", "Rscript"]`), but the bundle is keyed only by the
    /// first, so typing an alias resolved to nothing.
    aliases: BTreeMap<String, String>,
}

/// Pull the root `names` out of a spec's raw JSON without deserializing the
/// rest of it. A full `serde_json` walk of all 1470 specs would undo the lazy
/// index that keeps startup at ~75ms.
fn probe_names(raw: &[u8]) -> Vec<String> {
    const KEY: &[u8] = b"\"names\":";
    let Some(pos) = raw
        .windows(KEY.len())
        .position(|window| window == KEY)
        .map(|p| p + KEY.len())
    else {
        return Vec::new();
    };
    // `names` deserializes from either a bare string or an array of strings.
    let mut stream =
        serde_json::Deserializer::from_slice(&raw[pos..]).into_iter::<serde_json::Value>();
    match stream.next() {
        Some(Ok(serde_json::Value::String(s))) => vec![s],
        Some(Ok(serde_json::Value::Array(items))) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Lazy-loading spec registry. Matches upstream inshellisense's
/// architecture: at startup we decompress the zstd bundle and build a
/// lightweight name→byte-offset index (just scanning top-level JSON
/// keys, no deep parsing). Individual specs are deserialized on first
/// `get()` and cached. This cuts startup from ~500ms (parse all 1470
/// specs) to ~150ms (decompress + index scan), with ~1ms per-spec
/// cost on first lookup.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn inner_mut(&mut self) -> &mut Inner {
        self.inner.get_mut().unwrap_or_else(|e| e.into_inner())
    }

    pub fn new_with_defaults() -> Self {
        let mut r = Self::default();
        r.load_embedded_lazy();
        // Curated hand-ported specs override the bundle.
        for spec in crate::curated::all() {
            let name = spec.name().to_string();
            let inner = r.inner_mut();
            inner.lazy.remove(&name);
            if !inner.specs.contains_key(&name) {
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
        let inner = self.inner_mut();
        for (key, raw) in map {
            let bytes = raw.get().as_bytes().to_vec();
            for alias in probe_names(&bytes).into_iter().skip(1) {
                if alias != key {
                    inner.aliases.insert(alias, key.clone());
                }
            }
            inner.lazy.insert(key, bytes);
        }
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

    /// Load JSON specs from `root`, overriding anything already registered.
    /// Lets callers (notably tests) point at a spec directory explicitly
    /// instead of mutating the process-global `INSH_RS_SPECS_DIR`.
    pub fn load_spec_dir(&mut self, root: &std::path::Path) {
        self.load_disk_specs(root);
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
                        reg.inner_mut().specs.insert(key, Box::new(spec));
                        continue;
                    }
                }
                reg.insert(spec);
            }
        }
    }

    /// Register a spec under every one of its root names.
    ///
    /// Keying only on `names[0]` left 48 bundled specs unreachable by their
    /// aliases — typing `R`, `Rscript`, or `StepZen` resolved to nothing even
    /// though the spec declares those names.
    pub fn insert(&mut self, spec: Subcommand) {
        let names = spec.names.clone();
        let inner = self.inner_mut();
        let Some((primary, aliases)) = names.split_first() else {
            return;
        };
        for alias in aliases {
            inner.specs.insert(alias.clone(), Box::new(spec.clone()));
        }
        inner.specs.insert(primary.clone(), Box::new(spec));
    }

    /// Look up a spec by command name. If the spec hasn't been parsed
    /// yet (still in the lazy index), parse it now and cache it.
    /// Uses `&self` so callers don't need mutable access — the maps are
    /// behind a Mutex for interior mutability.
    pub fn get(&self, name: &str) -> Option<&Subcommand> {
        let mut inner = self.lock();
        if !inner.specs.contains_key(name) {
            // A root alias resolves to the primary key the bundle is keyed by.
            let key = if inner.lazy.contains_key(name) {
                name.to_string()
            } else {
                inner.aliases.get(name)?.clone()
            };
            if let Some(spec) = inner.specs.get(&key) {
                let spec = spec.clone();
                inner.specs.insert(name.to_string(), spec);
            } else {
                let raw = inner.lazy.remove(&key)?;
                match serde_json::from_slice::<Subcommand>(&raw) {
                    Ok(mut spec) => {
                        // Surgically augment bundled specs to close extractor
                        // gaps (missing options, opaque-JS generators, lost
                        // fields).
                        crate::curated::patch(&mut spec, &key);
                        if key != name {
                            inner.specs.insert(key, Box::new(spec.clone()));
                        }
                        inner.specs.insert(name.to_string(), Box::new(spec));
                    }
                    Err(e) => {
                        eprintln!("is: failed to parse spec `{}`: {}", key, e);
                        return None;
                    }
                }
            }
        }
        let spec: *const Subcommand = &**inner.specs.get(name)?;
        // SAFETY: `spec` points into a `Box` owned by `inner.specs`, so its
        // address is stable across later map rebalancing. Entries are only
        // ever added through `&self`; every path that removes or replaces one
        // (`insert`, `walk_disk`) takes `&mut self`, which borrowck proves
        // cannot run while the returned `&'a Subcommand` is alive. The
        // allocation therefore outlives the borrow of `self`, and the value
        // is never mutated in place, so handing out shared references —
        // possibly from several threads — does not alias a `&mut`.
        Some(unsafe { &*spec })
    }

    /// Returns all registered names (both parsed and lazy).
    pub fn names(&self) -> Vec<String> {
        let inner = self.lock();
        let mut names: Vec<String> = inner.specs.keys().cloned().collect();
        names.extend(inner.lazy.keys().cloned());
        names.extend(inner.aliases.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    pub fn len(&self) -> usize {
        self.names().len()
    }

    pub fn is_empty(&self) -> bool {
        let inner = self.lock();
        inner.specs.is_empty() && inner.lazy.is_empty()
    }

    /// Test-only hook: seed the lazy index directly so tests can exercise
    /// the deserialize-on-first-`get` path without the embedded bundle.
    #[cfg(test)]
    fn insert_lazy(&mut self, name: &str, json: &str) {
        self.inner_mut()
            .lazy
            .insert(name.to_string(), json.as_bytes().to_vec());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_registry(count: usize) -> Registry {
        let mut reg = Registry::default();
        for i in 0..count {
            reg.insert_lazy(
                &format!("cmd{i:04}"),
                &format!(r#"{{"names":"cmd{i:04}","description":"spec {i}"}}"#),
            );
        }
        reg
    }

    /// `get()` hands out `&Subcommand` borrowed from inside the map. Parsing
    /// a *different* spec afterwards inserts into that same map, which
    /// rebalances its nodes. Boxing the values keeps earlier references
    /// pointing at live, unmoved specs.
    ///
    /// The held spec sorts *after* everything inserted later: a `BTreeMap`
    /// leaf split promotes its middle entry into a new root, physically
    /// moving that value. Holding the *lowest* key would not move it and so
    /// would not exercise the hazard.
    #[test]
    fn references_survive_later_lazy_inserts() {
        let mut reg = lazy_registry(512);
        reg.insert_lazy("zzzz", r#"{"names":"zzzz","description":"held"}"#);
        let held = reg.get("zzzz").expect("held spec");
        // Force node splits below `zzzz` while it is still borrowed.
        for i in 0..512 {
            let name = format!("cmd{i:04}");
            assert_eq!(reg.get(&name).expect("spec").name(), name);
        }
        assert_eq!(held.name(), "zzzz");
        assert_eq!(held.description.as_deref(), Some("held"));
    }

    /// `Registry` is `Sync` and `get()` takes `&self`, so concurrent lookups
    /// from a shared reference must not race on the interior maps.
    #[test]
    fn concurrent_get_is_race_free() {
        let reg = lazy_registry(256);
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let reg = &reg;
                scope.spawn(move || {
                    for i in 0..256 {
                        // Stagger start offsets so threads contend on the
                        // same keys in different orders.
                        let idx = (i + thread * 32) % 256;
                        let name = format!("cmd{idx:04}");
                        assert_eq!(reg.get(&name).expect("spec").name(), name);
                    }
                });
            }
        });
        assert_eq!(reg.get("cmd0000").expect("spec").name(), "cmd0000");
    }

    /// A spec whose JSON fails to parse must not be retried forever, and
    /// must not resurrect as a `None`-shaped hole in `names()`.
    #[test]
    fn unparsable_lazy_spec_returns_none() {
        let mut reg = Registry::default();
        reg.insert_lazy("broken", "{ this is not json");
        assert!(reg.get("broken").is_none());
        assert!(reg.get("broken").is_none());
    }
}
