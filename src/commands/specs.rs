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
    // Registry keys include nested path-form entries for specs loaded
    // via subdirectories (e.g. `aws/ec2`, `gcloud/alpha/compute`).
    // Those exist for `LoadSpec::SpecPath` resolution at runtime, but
    // they're not standalone commands a user would invoke — `specs
    // list` should only surface the top-level primary names. Match
    // upstream's behavior exactly.
    let names: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|n| !n.contains('/'))
        .filter(|n| !n.is_empty() && n != "-")
        .collect();
    if plain {
        for name in &names {
            println!("{}", name);
        }
    } else {
        println!("{}", serde_json::to_string(&names)?);
    }
    Ok(())
}
