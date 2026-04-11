//! Hand-ported fallback specs.
//!
//! Phase 6.2 closed enough extractor gaps that git, cargo, docker,
//! systemctl, ssh, and every other essential is now extractor-sourced.
//! This file is kept as the documented escape hatch for adding
//! Rust-native specs in the future without round-tripping through the
//! extractor pipeline. Currently empty.

use crate::spec::model::Subcommand;

pub fn all() -> Vec<Subcommand> {
    Vec::new()
}
