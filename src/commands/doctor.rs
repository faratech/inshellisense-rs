//! `insh doctor` — thin pass-through to the three-check doctor suite
//! in `src/doctor.rs`.

use anyhow::Result;

pub fn run() -> Result<()> {
    crate::doctor::run()
}
