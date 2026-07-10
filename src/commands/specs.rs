//! `is specs list` — enumerate loaded spec names as JSON or plain text.
//!
//! Port of upstream inshellisense's `src/commands/specs/list.ts`. Upstream emits
//! a JSON array to stdout; we match that format exactly so scripts written
//! against upstream keep working. A `--plain` flag returns one-per-line
//! output which is friendlier for manual inspection. The `--shell` flag
//! includes shell aliases in the output (upstream: list.ts:24-26).

use crate::shell::Shell;
use crate::spec;
use anyhow::Result;

pub fn list(plain: bool, shell: Option<Shell>) -> Result<()> {
    let registry = spec::Registry::new_with_defaults();
    let mut names: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|n| !n.contains('/'))
        .filter(|n| !n.is_empty() && n != "-")
        .collect();
    // If --shell is given, prepend alias names (upstream: list.ts:24-26).
    if let Some(sh) = shell {
        let aliases = crate::alias::load(sh);
        let mut alias_names: Vec<String> = aliases.into_keys().collect();
        alias_names.sort();
        alias_names.append(&mut names);
        names = alias_names;
    }
    if plain {
        for name in &names {
            println!("{}", name);
        }
    } else {
        println!("{}", serde_json::to_string(&names)?);
    }
    Ok(())
}
