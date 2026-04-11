//! `insh uninstall` — remove cached resources, preserving user config.
//!
//! Port of `/tmp/inshellisense/src/commands/uninstall.ts`. Deletes the
//! `~/.insh-rs/` resource tree (log/, shell/, init/, spec/, version.txt)
//! but leaves `~/.config/insh-rs/` (user config, key bindings, user
//! specs) intact. Idempotent — re-running is safe.
//!
//! P1 stub; P2 provides the full path list via `src/paths.rs`.

use anyhow::Result;

pub fn run() -> Result<()> {
    let Some(home) = dirs::home_dir() else {
        println!("insh-rs: no HOME — nothing to remove");
        return Ok(());
    };
    let resource_root = home.join(".insh-rs");
    if !resource_root.exists() {
        println!("insh-rs: {} not present — nothing to do", resource_root.display());
        println!("insh-rs: user config at ~/.config/insh-rs/ preserved");
        return Ok(());
    }
    match std::fs::remove_dir_all(&resource_root) {
        Ok(()) => {
            println!("insh-rs: removed {}", resource_root.display());
            println!("insh-rs: user config at ~/.config/insh-rs/ preserved");
            println!("insh-rs: run `cargo uninstall insh-rs` to remove the binary");
        }
        Err(e) => {
            eprintln!("insh-rs: failed to remove {}: {}", resource_root.display(), e);
        }
    }
    Ok(())
}
