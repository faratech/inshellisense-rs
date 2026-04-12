//! `insh uninstall` — remove cached resources, preserving user config.
//!
//! Port of `/tmp/inshellisense/src/commands/uninstall.ts`. Deletes the
//! `~/.inshellisense/` resource tree (log/, shell/, init/, zsh-dotdir/, spec/,
//! version.txt) but leaves `~/.config/inshellisense/` (user config, key
//! bindings, user specs) intact. Idempotent — re-running is safe.

use crate::{paths, resources};
use anyhow::Result;

pub fn run() -> Result<()> {
    let Some(root) = paths::resource_root() else {
        println!("is: no HOME — nothing to remove");
        return Ok(());
    };
    if !root.exists() {
        println!("• {} not present — nothing to do", root.display());
        println!("  user config at ~/.config/inshellisense/ preserved");
        return Ok(());
    }
    resources::remove_all()?;
    println!("✓ removed {}", root.display());
    println!("  user config at ~/.config/inshellisense/ preserved");
    println!("  run `cargo uninstall inshellisense-rs` to remove the binary");
    Ok(())
}
