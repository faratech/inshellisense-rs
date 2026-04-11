//! insh-rs library crate — exposes the internal modules so integration
//! tests and external consumers can drive the suggestion engine, spec
//! registry, parser, and generator runtime directly.
//!
//! The binary (`src/main.rs`) is thin CLI plumbing that uses this crate.

pub mod ansi;
pub mod curated;
pub mod generator;
pub mod history;
pub mod pty;
pub mod render;
pub mod shell_init;
pub mod spec;
pub mod suggest;
pub mod term;
