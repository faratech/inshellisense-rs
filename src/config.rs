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
    #[serde(alias = "useAliases")]
    pub use_aliases: bool,
    #[serde(alias = "useNerdFont")]
    pub use_nerd_font: bool,
    #[serde(alias = "maxSuggestions")]
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
            ui: UiMode::Hybrid,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Bindings {
    #[serde(alias = "nextSuggestion")]
    pub next_suggestion: KeyBinding,
    #[serde(alias = "previousSuggestion")]
    pub previous_suggestion: KeyBinding,
    #[serde(alias = "acceptSuggestion")]
    pub accept_suggestion: KeyBinding,
    #[serde(alias = "dismissSuggestions")]
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

    /// Does a raw stdin chunk match this binding?
    ///
    /// Terminal input is byte-oriented: plain keys map to one byte
    /// (`\t`, `\r`, `\x7f`), CSI sequences map to a 3+ byte escape
    /// burst (`\x1b[A` etc.). We do a straight byte-string compare
    /// against whichever sequence(s) the symbolic name represents.
    ///
    /// Lone-escape caveat: an `Esc` key press is delivered as a single
    /// `\x1b` byte, but CSI sequences also start with `\x1b`. Real
    /// terminals deliver the full CSI burst in one write(2), so we
    /// only treat a chunk as "escape" when it is *exactly* `\x1b`.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        match self.key.as_str() {
            "up" => bytes == b"\x1b[A" || bytes == b"\x1bOA",
            "down" => bytes == b"\x1b[B" || bytes == b"\x1bOB",
            "right" => bytes == b"\x1b[C" || bytes == b"\x1bOC",
            "left" => bytes == b"\x1b[D" || bytes == b"\x1bOD",
            "home" => bytes == b"\x1b[H" || bytes == b"\x1b[1~",
            "end" => bytes == b"\x1b[F" || bytes == b"\x1b[4~",
            "tab" => bytes == b"\t",
            "escape" | "esc" => bytes == b"\x1b",
            "return" | "enter" => bytes == b"\r" || bytes == b"\n",
            "backspace" => bytes == b"\x7f" || bytes == b"\x08",
            "space" => bytes == b" ",
            other => {
                // Single-char literal binding like "a" or "?".
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(_), None) => bytes == other.as_bytes(),
                    _ => false,
                }
            }
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
    /// Grey ghost text inline after the cursor — PSReadLine style.
    Ghost,
    /// Interactive popup below/above the prompt with navigation.
    Popup,
    /// Both at once: grey ghost text for the top suggestion plus a
    /// navigable popup for the alternatives. Warp / fish / atuin style.
    /// This is the default when no `ui` is set.
    #[default]
    Hybrid,
}

impl UiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            UiMode::Ghost => "ghost",
            UiMode::Popup => "popup",
            UiMode::Hybrid => "hybrid",
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
                    "is: {} is invalid TOML: {}",
                    path.display(),
                    e
                );
                None
            }
        },
        Err(e) => {
            eprintln!("is: failed to read {}: {}", path.display(), e);
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
    fn default_has_hybrid_ui() {
        let c = Config::default();
        assert_eq!(c.ui, UiMode::Hybrid);
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
    fn key_binding_matches_common_keys() {
        assert!(KeyBinding::new("up").matches(b"\x1b[A"));
        assert!(KeyBinding::new("down").matches(b"\x1b[B"));
        assert!(KeyBinding::new("right").matches(b"\x1b[C"));
        assert!(KeyBinding::new("left").matches(b"\x1b[D"));
        assert!(KeyBinding::new("tab").matches(b"\t"));
        assert!(KeyBinding::new("escape").matches(b"\x1b"));
        assert!(!KeyBinding::new("escape").matches(b"\x1b[A"));
        assert!(KeyBinding::new("return").matches(b"\r"));
        assert!(KeyBinding::new("return").matches(b"\n"));
        assert!(KeyBinding::new("backspace").matches(b"\x7f"));
        assert!(KeyBinding::new("backspace").matches(b"\x08"));
        assert!(!KeyBinding::new("down").matches(b"\x1b[A"));
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
