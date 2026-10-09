//! inshellisense-rs library crate — exposes the internal modules so integration
//! tests and external consumers can drive the suggestion engine, spec
//! registry, parser, and generator runtime directly.
//!
//! The binary (`src/main.rs`) is thin CLI plumbing that uses this crate.

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub mod alias;
pub mod ansi;
pub mod commands;
pub mod config;
pub mod coreutils;
pub mod curated;
pub mod doctor;
pub mod env;
pub mod generator;
pub mod history;
pub mod parity;
pub mod paths;
pub mod platform;
pub mod pty;
pub mod render;
pub mod resources;
pub mod shell;
pub mod shell_init;
pub mod spec;
pub mod suggest;
pub mod term;

/// Scratch directories for unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    /// A fresh, uniquely named directory under the system temp dir.
    ///
    /// Created with `create_dir`, not `create_dir_all`, so a test fails
    /// instead of adopting a directory someone else pre-created at a
    /// predictable name in the shared temp dir.
    pub fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "insh-{tag}-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
}
