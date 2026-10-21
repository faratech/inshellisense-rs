//! `is complete <line>` — offline completion query.
//!
//! Default output is JSON matching upstream's exact schema:
//! `{"suggestions":[...],"activeToken":{...}}`

use crate::{
    render::popup::{IconSet, icon_for},
    shell::Shell,
    spec, suggest,
};
use anyhow::Result;

pub fn run(line: &str, text_mode: bool, cwd: &str, shell: Option<Shell>) -> Result<()> {
    let registry = spec::Registry::new_with_defaults();
    let mut engine = suggest::Engine::new(registry, Vec::new());
    // Offline query: no live shell-session history (matches upstream).
    engine.set_offline(true);
    engine.set_shell(shell.unwrap_or_else(crate::shell::detect));

    if text_mode {
        // The tail can be the rest of a filename or generator value; print it
        // with its control characters made visible, as the ghost renderer
        // does. (JSON mode is already safe: serde escapes them.)
        if let Some(s) = engine.suggest(line, cwd) {
            println!("{}", crate::render::printable(&s));
        }
        return Ok(());
    }

    // JSON output matching upstream's schema exactly.
    let icons = IconSet::from_config(crate::config::load().use_nerd_font);
    // Upstream emits a null activeToken once the last token is complete
    // (i.e. the line ends in whitespace) — only an in-progress token counts.
    let (blob, active_token) = engine.suggest_blob_with_token(line, cwd);
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

    let active_token_json = active_token_json(active_token);

    let output = serde_json::json!({
        "suggestions": suggestions,
        "activeToken": active_token_json,
    });

    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn active_token_json(active_token: Option<spec::CommandToken>) -> Option<serde_json::Value> {
    active_token.map(|t| {
        serde_json::json!({
            "token": t.token,
            "tokenLength": t.token_length,
            "complete": t.complete,
            "isOption": t.is_option,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_active_token_uses_resolved_required_value_and_raw_span() {
        let mut registry = spec::Registry::default();
        registry.insert(spec::Subcommand {
            names: vec!["audit".into()],
            options: vec![spec::Opt {
                names: vec!["-e".into()],
                args: vec![spec::Arg::default()],
                ..Default::default()
            }],
            ..Default::default()
        });
        let engine = suggest::Engine::new(registry, Vec::new());
        for (line, length) in [("audit -e --flag=va", 9), ("audit -e '--flag=va'", 11)] {
            let (_, token) = engine.suggest_blob_with_token(line, ".");
            assert_eq!(
                active_token_json(token),
                Some(serde_json::json!({
                    "token": "--flag=va", "tokenLength": length,
                    "complete": false, "isOption": false,
                })),
                "{line}"
            );
        }
        let (_, token) = engine.suggest_blob_with_token("audit -e --flag=value ", ".");
        assert_eq!(active_token_json(token), None);
    }
}
