//! Per-command handlers. Broken out of src/main.rs so each command's
//! behavior is co-located with its documentation and can be tested in
//! isolation.

pub mod complete;
pub mod doctor;
pub mod reinit;
pub mod specs;
pub mod uninstall;
