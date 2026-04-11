//! `insh doctor` — minimal viable version in P1.
//!
//! P5 will expand this with the full upstream parity suite (legacy config
//! scan, shell-plugin last-line check, per-shell config existence). For
//! now, keep the informational output so the command is usable.

use crate::{history, spec};
use anyhow::Result;

pub fn run() -> Result<()> {
    println!("insh-rs doctor");
    println!("  bash present: {}", which("bash"));
    println!("  zsh present:  {}", which("zsh"));
    println!("  fish present: {}", which("fish"));
    println!("  HOME: {:?}", dirs::home_dir());
    println!("  bash history entries: {}", history::load().len());
    let specs = spec::Registry::new_with_defaults();
    println!("  specs loaded: {}", specs.len());
    println!(
        "  session active: {}",
        if crate::env::session_active() { "yes" } else { "no" }
    );
    println!("  js runtime: none (pure Rust build)");
    Ok(())
}

fn which(cmd: &str) -> bool {
    std::env::var("PATH")
        .ok()
        .map(|p| {
            p.split(':')
                .any(|dir| std::path::Path::new(dir).join(cmd).exists())
        })
        .unwrap_or(false)
}
