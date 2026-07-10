//! `is complete <line>` — offline completion query.
//!
//! Default output is JSON matching upstream's exact schema:
//! `{"suggestions":[...],"activeToken":{...}}`

use crate::{
    history,
    render::popup::{IconSet, icon_for},
    shell::Shell,
    spec, suggest,
};
use anyhow::Result;

pub fn run(line: &str, text_mode: bool, cwd: &str, shell: Option<Shell>) -> Result<()> {
    let hist = history::load();
    let registry = spec::Registry::new_with_defaults();
    let mut engine = suggest::Engine::new(registry, hist);
    // Offline query: no live shell-session history (matches upstream).
    engine.set_offline(true);
    engine.set_shell(shell.unwrap_or_else(crate::shell::detect));

    if text_mode {
        if let Some(s) = engine.suggest(line, cwd) {
            println!("{}", s);
        }
        return Ok(());
    }

    // JSON output matching upstream's schema exactly.
    let icons = IconSet::from_config(crate::config::load().use_nerd_font);
    let blob = engine.suggest_blob(line, cwd);
    let tokens = spec::parse_command(line);
    // Upstream emits a null activeToken once the last token is complete
    // (i.e. the line ends in whitespace) — only an in-progress token counts.
    let active_token = tokens.last().filter(|t| !t.complete);

    let suggestions: Vec<serde_json::Value> = blob
        .into_iter()
        .map(|s| {
            let icon = s
                .icon
                .as_deref()
                .unwrap_or_else(|| icon_for(&s, icons))
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
                "type": format!("{:?}", s.suggestion_type).to_lowercase(),
            })
        })
        .collect();

    let active_token_json = active_token.map(|t| {
        serde_json::json!({
            "token": t.token,
            "tokenLength": t.token_length,
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
