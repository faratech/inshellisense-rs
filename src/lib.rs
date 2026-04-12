//! inshellisense-rs library crate — exposes the internal modules so integration
//! tests and external consumers can drive the suggestion engine, spec
//! registry, parser, and generator runtime directly.
//!
//! The binary (`src/main.rs`) is thin CLI plumbing that uses this crate.

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub mod ansi;
pub mod commands;
pub mod config;
pub mod curated;
pub mod doctor;
pub mod env;
pub mod generator;
pub mod history;
pub mod parity;
pub mod platform;
pub mod paths;
pub mod pty;
pub mod render;
pub mod resources;
pub mod shell;
pub mod shell_init;
pub mod spec;
pub mod suggest;
pub mod term;
