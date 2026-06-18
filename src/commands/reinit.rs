//! `is reinit` — regenerate all shell configs and re-unpack resources.
//!
//! Port of `/tmp/inshellisense/src/commands/reinit.ts`. Forces a refresh
//! of the `~/.inshellisense/` resource tree after an inshellisense-rs upgrade. Because
//! `resources::unpack()` is version-gated, we delete the version file
//! first so the unpacker runs unconditionally.

use crate::{paths, resources};
use anyhow::Result;
use std::fs;

pub fn run() -> Result<()> {
    if let Some(v) = paths::version_file() {
        let _ = fs::remove_file(&v);
    }
    let root = resources::unpack()?;
    println!("✓ inshellisense-rs resources regenerated at {}", root.display());
    Ok(())
}
