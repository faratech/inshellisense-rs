//! `insh specs list` — enumerate loaded spec names as JSON or plain text.
//!
//! Port of `/tmp/inshellisense/src/commands/specs/list.ts`. Upstream emits
//! a JSON array to stdout; we match that format exactly so scripts written
//! against upstream keep working. A `--plain` flag returns one-per-line
//! output which is friendlier for manual inspection.

use crate::spec;
use anyhow::Result;

pub fn list(plain: bool) -> Result<()> {
    let registry = spec::Registry::new_with_defaults();
    let names: Vec<&str> = registry.names().collect();
    if plain {
        for name in &names {
            println!("{}", name);
        }
    } else {
        println!("{}", serde_json::to_string(&names)?);
    }
    Ok(())
}
