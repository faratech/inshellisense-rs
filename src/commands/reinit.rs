//! `insh reinit` — regenerate all shell configs and re-unpack resources.
//!
//! Port of `/tmp/inshellisense/src/commands/reinit.ts`. Calling `reinit`
//! forces a refresh of the `~/.insh-rs/` resource tree after an insh-rs
//! upgrade. In P1 this is a no-op stub that prints a message; P2 wires it
//! to the resource unpacker, P3 iterates all 7 shells.

use anyhow::Result;

pub fn run() -> Result<()> {
    println!("insh-rs: reinit (P1 stub — full implementation in P2/P3)");
    Ok(())
}
