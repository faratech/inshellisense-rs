//! `is complete <line>` — offline completion query.
//!
//! Default output is JSON matching upstream's exact schema:
//! `{"suggestions":[...],"activeToken":{...}}`

use crate::{history, render::popup::icon_for, spec, suggest};
use anyhow::Result;

pub fn run(line: &str, text_mode: bool, cwd: &str) -> Result<()> {
    let hist = history::load();
    let registry = spec::Registry::new_with_defaults();
    let engine = suggest::Engine::new(registry, hist);

    if text_mode {
        if let Some(s) = engine.suggest(line, cwd) {
            println!("{}", s);
        }
        return Ok(());
    }

    // JSON output matching upstream's schema exactly.
    let blob = engine.suggest_blob(line, cwd);
    let tokens = spec::parse_command(line);
    let active_token = tokens.last();

    let suggestions: Vec<serde_json::Value> = blob
        .into_iter()
        .map(|s| {
            let icon = s
                .icon
                .as_deref()
                .unwrap_or_else(|| icon_for(&s))
                .to_string();
            let all_names: Vec<&str> = s.all_names.iter().map(|n| n.as_str()).collect();
            let names = if all_names.is_empty() {
                vec![s.name.as_str()]
            } else {
                all_names
            };
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "icon": icon,
                "allNames": names,
                "priority": s.priority.unwrap_or(50),
                "insertValue": s.insert_value,
                "type": format!("{:?}", s.suggestion_type).to_lowercase(),
                "hidden": s.hidden,
            })
        })
        .collect();

    let active_token_json = active_token.map(|t| {
        serde_json::json!({
            "token": t.token,
            "tokenLength": t.token.len(),
            "complete": t.complete,
            "isOption": t.is_option,
        })
    });

    let output = serde_json::json!({
        "suggestions": suggestions,
        "activeToken": active_token_json,
    });

    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
