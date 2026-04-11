//! Global config file loader — parity port of
//! `/tmp/inshellisense/src/utils/config.ts`.
//!
//! Load order (last one wins per field):
//!   1. `~/.inshellisenserc` (upstream, for compat)
//!   2. `~/.config/inshellisense/rc.toml` (upstream XDG, for compat)
//!   3. `~/.config/insh-rs/rc.toml` (our own)
//!
//! Every file is optional; missing files fall back to defaults. Invalid
//! TOML produces a readable error via anyhow instead of aborting.
//!
//! Schema (matches upstream exactly):
//!
//! ```toml
//! [bindings]
//! next_suggestion     = { key = "down",   shift = false, control = false }
//! previous_suggestion = { key = "up",     shift = false, control = false }
//! accept_suggestion   = { key = "tab",    shift = false, control = false }
//! dismiss_suggestions = { key = "escape", shift = false, control = false }
//!
//! [specs]
//! path = ["/extra/spec/dir"]
//!
//! use_aliases     = false
//! use_nerd_font   = false
//! max_suggestions = 5
//! ui              = "ghost"  # or "popup"
//! ```

use crate::paths;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bindings: Bindings,
    pub specs: SpecsConfig,
    pub use_aliases: bool,
    pub use_nerd_font: bool,
    pub max_suggestions: u8,
    pub ui: UiMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: Bindings::default(),
            specs: SpecsConfig::default(),
            use_aliases: false,
            use_nerd_font: false,
            max_suggestions: 5,
            ui: UiMode::Ghost,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Bindings {
    pub next_suggestion: KeyBinding,
    pub previous_suggestion: KeyBinding,
    pub accept_suggestion: KeyBinding,
    pub dismiss_suggestions: KeyBinding,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            next_suggestion: KeyBinding::new("down"),
            previous_suggestion: KeyBinding::new("up"),
            accept_suggestion: KeyBinding::new("tab"),
            dismiss_suggestions: KeyBinding::new("escape"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SpecsConfig {
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeyBinding {
    pub key: String,
    pub shift: bool,
    pub control: bool,
}

impl KeyBinding {
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_string(),
            shift: false,
            control: false,
        }
    }
}

impl Default for KeyBinding {
    fn default() -> Self {
        Self::new("")
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "lowercase")]
pub enum UiMode {
    #[default]
    Ghost,
    Popup,
}

impl UiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            UiMode::Ghost => "ghost",
            UiMode::Popup => "popup",
        }
    }
}

/// Load config from all known paths. Never fails — invalid files log a
/// warning to stderr and are skipped so a botched config can't brick
/// the CLI.
pub fn load() -> Config {
    let mut cfg = Config::default();
    for path in candidate_paths() {
        if let Some(loaded) = try_load(&path) {
            cfg = merge(cfg, loaded);
        }
    }
    cfg
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = paths::upstream_config_files();
    if let Some(p) = paths::user_config_file() {
        out.push(p);
    }
    out
}

fn try_load(path: &PathBuf) -> Option<Config> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!(
                    "insh-rs: {} is invalid TOML: {}",
                    path.display(),
                    e
                );
                None
            }
        },
        Err(e) => {
            eprintln!("insh-rs: failed to read {}: {}", path.display(), e);
            None
        }
    }
}

/// Merge two configs — fields from `override_` win over `base`. The
/// merge is shallow (we don't diff `bindings` field-by-field); whoever
/// sets a whole-table entry last wins for that table.
fn merge(base: Config, override_: Config) -> Config {
    // Strategy: toml::Value-based merge would be cleaner, but for our
    // limited schema a field-by-field pick is fine.
    Config {
        bindings: Bindings {
            next_suggestion: pick_key(
                base.bindings.next_suggestion,
                override_.bindings.next_suggestion,
            ),
            previous_suggestion: pick_key(
                base.bindings.previous_suggestion,
                override_.bindings.previous_suggestion,
            ),
            accept_suggestion: pick_key(
                base.bindings.accept_suggestion,
                override_.bindings.accept_suggestion,
            ),
            dismiss_suggestions: pick_key(
                base.bindings.dismiss_suggestions,
                override_.bindings.dismiss_suggestions,
            ),
        },
        specs: SpecsConfig {
            path: if override_.specs.path.is_empty() {
                base.specs.path
            } else {
                override_.specs.path
            },
        },
        use_aliases: override_.use_aliases || base.use_aliases,
        use_nerd_font: override_.use_nerd_font || base.use_nerd_font,
        max_suggestions: override_.max_suggestions,
        ui: override_.ui,
    }
}

fn pick_key(base: KeyBinding, override_: KeyBinding) -> KeyBinding {
    if override_.key.is_empty() {
        base
    } else {
        override_
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_ghost_ui() {
        let c = Config::default();
        assert_eq!(c.ui, UiMode::Ghost);
        assert_eq!(c.max_suggestions, 5);
        assert_eq!(c.bindings.accept_suggestion.key, "tab");
    }

    #[test]
    fn deserialize_full_config() {
        let toml_src = r#"
use_aliases = true
use_nerd_font = false
max_suggestions = 3
ui = "popup"

[bindings.accept_suggestion]
key = "right"
shift = false
control = false

[specs]
path = ["/tmp/extra"]
"#;
        let c: Config = toml::from_str(toml_src).expect("parse");
        assert!(c.use_aliases);
        assert_eq!(c.max_suggestions, 3);
        assert_eq!(c.ui, UiMode::Popup);
        assert_eq!(c.bindings.accept_suggestion.key, "right");
        assert_eq!(c.specs.path, vec!["/tmp/extra".to_string()]);
    }

    #[test]
    fn merge_override_wins_for_ui() {
        let base = Config::default();
        let over = Config {
            ui: UiMode::Popup,
            max_suggestions: 8,
            ..Config::default()
        };
        let merged = merge(base, over);
        assert_eq!(merged.ui, UiMode::Popup);
        assert_eq!(merged.max_suggestions, 8);
    }
}
