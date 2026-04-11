//! `is` — upstream-compatible alias for the `insh` binary.
//!
//! Microsoft's inshellisense ships both `inshellisense` and `is`; we match
//! that by shipping `insh` and `is` from the same `cargo install`. This
//! file is a thin wrapper that #[path]-imports the full CLI from main.rs
//! so we don't duplicate any code.

#[path = "../main.rs"]
mod main_impl;

fn main() -> anyhow::Result<()> {
    main_impl::main()
}
