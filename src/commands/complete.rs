//! `insh complete <line>` — offline completion query.

use crate::{history, spec, suggest};
use anyhow::Result;

pub fn run(line: &str, json: bool, cwd: &str) -> Result<()> {
    let hist = history::load();
    let registry = spec::Registry::new_with_defaults();
    let engine = suggest::Engine::new(registry, hist);
    if json {
        let blob = engine.suggest_blob(line, cwd);
        let printable: Vec<serde_json::Value> = blob
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "display_name": s.display_name,
                    "insert_value": s.insert_value,
                    "description": s.description,
                    "icon": s.icon,
                    "type": format!("{:?}", s.suggestion_type).to_lowercase(),
                    "priority": s.priority,
                    "hidden": s.hidden,
                    "deprecated": s.deprecated,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&printable)?);
        return Ok(());
    }
    if let Some(s) = engine.suggest(line, cwd) {
        println!("{}", s);
    }
    Ok(())
}
