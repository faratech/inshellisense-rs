//! Global config file loader — parity port of
//! upstream inshellisense's `src/utils/config.ts`.
//!
//! Load order (last one wins per field):
//!   1. `~/.inshellisenserc` (upstream, for compat)
//!   2. `~/.config/inshellisense/rc.toml` (upstream XDG, for compat)
//!   3. `~/.config/inshellisense-rs/rc.toml` (our own)
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

/// Keys the schema does not define. Captured rather than dropped so a typo
/// like `maxSuggestion` is reported instead of silently doing nothing.
/// Loading stays non-fatal: an unknown key warns, it does not brick the CLI.
type Unknown = std::collections::BTreeMap<String, toml::Value>;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialConfig {
    bindings: PartialBindings,
    specs: PartialSpecsConfig,
    #[serde(alias = "useAliases")]
    use_aliases: Option<bool>,
    #[serde(alias = "useNerdFont")]
    use_nerd_font: Option<bool>,
    #[serde(alias = "maxSuggestions")]
    max_suggestions: Option<u8>,
    ui: Option<UiMode>,
    #[serde(flatten)]
    unknown: Unknown,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialBindings {
    #[serde(alias = "nextSuggestion")]
    next_suggestion: Option<PartialKeyBinding>,
    #[serde(alias = "previousSuggestion")]
    previous_suggestion: Option<PartialKeyBinding>,
    #[serde(alias = "acceptSuggestion")]
    accept_suggestion: Option<PartialKeyBinding>,
    #[serde(alias = "dismissSuggestions")]
    dismiss_suggestions: Option<PartialKeyBinding>,
    #[serde(flatten)]
    unknown: Unknown,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialSpecsConfig {
    path: Option<Vec<String>>,
    #[serde(flatten)]
    unknown: Unknown,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialKeyBinding {
    key: Option<String>,
    shift: Option<bool>,
    control: Option<bool>,
    #[serde(flatten)]
    unknown: Unknown,
}

impl PartialConfig {
    /// Fully-qualified names of every key the schema does not define.
    fn unknown_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self.unknown.keys().cloned().collect();
        out.extend(self.specs.unknown.keys().map(|k| format!("specs.{k}")));
        out.extend(
            self.bindings
                .unknown
                .keys()
                .map(|k| format!("bindings.{k}")),
        );
        let bindings = [
            ("next_suggestion", &self.bindings.next_suggestion),
            ("previous_suggestion", &self.bindings.previous_suggestion),
            ("accept_suggestion", &self.bindings.accept_suggestion),
            ("dismiss_suggestions", &self.bindings.dismiss_suggestions),
        ];
        for (name, binding) in bindings {
            if let Some(binding) = binding {
                out.extend(
                    binding
                        .unknown
                        .keys()
                        .map(|k| format!("bindings.{name}.{k}")),
                );
            }
        }
        out.sort();
        out
    }
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
        // A binding with a `shift`/`control` modifier must match the
        // modifier-encoded byte sequence — terminals encode modifiers in the
        // sequence itself (shift+Down = `\x1b[1;2B`, Ctrl-N = `\x0e`), so the
        // plain key bytes must NOT satisfy a modified binding. Default
        // bindings carry no modifiers and take the plain path below.
        if self.shift || self.control {
            return self.matches_modified(bytes);
        }
        self.matches_plain(bytes)
    }

    fn matches_plain(&self, bytes: &[u8]) -> bool {
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

    /// Match a binding that carries a `shift` and/or `control` modifier.
    ///
    /// Arrow/Home/End keys use the xterm CSI form `\x1b[1;<mod><final>`
    /// where `<mod>` = 1 + shift(1) + control(4). Ctrl + a letter maps to
    /// the corresponding C0 control byte; Shift+Tab is backtab (`\x1b[Z`).
    fn matches_modified(&self, bytes: &[u8]) -> bool {
        let code = 1 + if self.shift { 1 } else { 0 } + if self.control { 4 } else { 0 };
        let csi = |fin: char| format!("\x1b[1;{code}{fin}").into_bytes();
        match self.key.as_str() {
            "up" => bytes == csi('A'),
            "down" => bytes == csi('B'),
            "right" => bytes == csi('C'),
            "left" => bytes == csi('D'),
            "home" => bytes == csi('H'),
            "end" => bytes == csi('F'),
            // Shift+Tab is backtab; Ctrl+Tab has no standard sequence.
            "tab" => self.shift && !self.control && bytes == b"\x1b[Z",
            other => {
                let mut chars = other.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return false;
                };
                // Ctrl + single ASCII letter → C0 control byte (Ctrl-A=0x01).
                if self.control && !self.shift {
                    if c.is_ascii_alphabetic() {
                        return bytes == [(c.to_ascii_uppercase() as u8) & 0x1f];
                    }
                    return false;
                }
                // Shift + a character key. A terminal does not report a shift
                // bit for character keys — it reports the *shifted character*.
                // So `{ key = "a", shift = true }` must match the byte(s) for
                // `A`. Without this a shift-only character binding could never
                // match anything. Keys with no distinct shifted form (`?`) are
                // accepted as themselves.
                if self.shift && !self.control {
                    let shifted: String = c.to_uppercase().collect();
                    return bytes == shifted.as_bytes() || bytes == other.as_bytes();
                }
                // Ctrl+Shift+<char> has no portable encoding.
                false
            }
        }
    }
}

impl Default for KeyBinding {
    fn default() -> Self {
        Self::new("")
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
            cfg = merge_partial(cfg, loaded);
        }
    }
    cfg
}

/// Human-readable problems with the on-disk config files: parse errors,
/// unreadable files, and keys the schema does not define. `load()` tolerates
/// all of these; `is doctor` reports them.
pub fn diagnose() -> Vec<String> {
    let mut problems = Vec::new();
    for path in candidate_paths() {
        if !path.exists() {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                problems.push(format!("{}: cannot read ({e})", path.display()));
                continue;
            }
        };
        match toml::from_str::<PartialConfig>(&text) {
            Ok(parsed) => problems.extend(
                parsed
                    .unknown_keys()
                    .into_iter()
                    .map(|key| format!("{}: unknown key `{key}`", path.display())),
            ),
            Err(e) => problems.push(format!("{}: invalid TOML ({e})", path.display())),
        }
    }
    problems
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = paths::upstream_config_files();
    if let Some(p) = paths::user_config_file() {
        out.push(p);
    }
    out
}

fn try_load(path: &PathBuf) -> Option<PartialConfig> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<PartialConfig>(&text) {
            Ok(c) => {
                for key in c.unknown_keys() {
                    eprintln!("is: {}: unknown config key `{}`", path.display(), key);
                }
                Some(c)
            }
            Err(e) => {
                eprintln!("is: {} is invalid TOML: {}", path.display(), e);
                None
            }
        },
        Err(e) => {
            eprintln!("is: failed to read {}: {}", path.display(), e);
            None
        }
    }
}

/// Merge a partially specified config into the accumulated effective config.
/// Only fields present in the file override earlier values.
fn merge_partial(mut base: Config, override_: PartialConfig) -> Config {
    merge_key(
        &mut base.bindings.next_suggestion,
        override_.bindings.next_suggestion,
    );
    merge_key(
        &mut base.bindings.previous_suggestion,
        override_.bindings.previous_suggestion,
    );
    merge_key(
        &mut base.bindings.accept_suggestion,
        override_.bindings.accept_suggestion,
    );
    merge_key(
        &mut base.bindings.dismiss_suggestions,
        override_.bindings.dismiss_suggestions,
    );
    if let Some(path) = override_.specs.path {
        base.specs.path = path;
    }
    if let Some(v) = override_.use_aliases {
        base.use_aliases = v;
    }
    if let Some(v) = override_.use_nerd_font {
        base.use_nerd_font = v;
    }
    if let Some(v) = override_.max_suggestions {
        base.max_suggestions = v;
    }
    if let Some(v) = override_.ui {
        base.ui = v;
    }
    base
}

fn merge_key(base: &mut KeyBinding, override_: Option<PartialKeyBinding>) {
    let Some(override_) = override_ else {
        return;
    };
    if let Some(key) = override_.key {
        base.key = key;
    }
    if let Some(shift) = override_.shift {
        base.shift = shift;
    }
    if let Some(control) = override_.control {
        base.control = control;
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

    /// A terminal reports the shifted *character*, not a shift bit, so a
    /// shift-only character binding must match the uppercase byte. Previously
    /// this fell through to `false` and could never match any input.
    #[test]
    fn shift_only_character_binding_matches_shifted_char() {
        let shift_a = KeyBinding {
            key: "a".into(),
            shift: true,
            control: false,
        };
        assert!(shift_a.matches(b"A"));
        assert!(!shift_a.matches(b"b"));
        // A key with no distinct shifted form still matches itself.
        let shift_question = KeyBinding {
            key: "?".into(),
            shift: true,
            control: false,
        };
        assert!(shift_question.matches(b"?"));
    }

    /// Unknown / misspelled keys are surfaced rather than silently ignored.
    #[test]
    fn unknown_config_keys_are_reported() {
        let text = r#"
maxSuggestion = 7
[specs]
paths = ["/x"]
[bindings]
acceptSuggestions = { key = "tab" }
[bindings.accept_suggestion]
key = "tab"
modifier = "ctrl"
"#;
        let parsed: PartialConfig = toml::from_str(text).unwrap();
        assert_eq!(
            parsed.unknown_keys(),
            vec![
                "bindings.acceptSuggestions",
                "bindings.accept_suggestion.modifier",
                "maxSuggestion",
                "specs.paths",
            ]
        );
        // A correctly-spelled config reports nothing.
        let ok: PartialConfig = toml::from_str(
            "max_suggestions = 7
",
        )
        .unwrap();
        assert!(ok.unknown_keys().is_empty());
    }

    #[test]
    fn key_binding_honors_modifiers() {
        let ctrl_down = KeyBinding {
            key: "down".into(),
            shift: false,
            control: true,
        };
        assert!(ctrl_down.matches(b"\x1b[1;5B")); // Ctrl+Down
        assert!(!ctrl_down.matches(b"\x1b[B")); // plain Down must NOT satisfy it
        let shift_up = KeyBinding {
            key: "up".into(),
            shift: true,
            control: false,
        };
        assert!(shift_up.matches(b"\x1b[1;2A")); // Shift+Up
        assert!(!shift_up.matches(b"\x1b[A"));
        let ctrl_n = KeyBinding {
            key: "n".into(),
            shift: false,
            control: true,
        };
        assert!(ctrl_n.matches(b"\x0e")); // Ctrl-N
        let ctrl_shift_n = KeyBinding {
            key: "n".into(),
            shift: true,
            control: true,
        };
        assert!(!ctrl_shift_n.matches(b"\x0e")); // no portable encoding
        let shift_tab = KeyBinding {
            key: "tab".into(),
            shift: true,
            control: false,
        };
        assert!(shift_tab.matches(b"\x1b[Z"));
        // A plain (unmodified) binding must NOT match a modified sequence.
        assert!(!KeyBinding::new("down").matches(b"\x1b[1;5B"));
    }

    #[test]
    fn merge_override_wins_for_ui() {
        let base = Config::default();
        let over = PartialConfig {
            ui: Some(UiMode::Popup),
            max_suggestions: Some(8),
            ..PartialConfig::default()
        };
        let merged = merge_partial(base, over);
        assert_eq!(merged.ui, UiMode::Popup);
        assert_eq!(merged.max_suggestions, 8);
    }

    #[test]
    fn partial_merge_preserves_missing_fields_and_allows_false() {
        let base = Config {
            use_aliases: true,
            max_suggestions: 8,
            ui: UiMode::Popup,
            ..Config::default()
        };
        let over = PartialConfig {
            use_aliases: Some(false),
            ..PartialConfig::default()
        };
        let merged = merge_partial(base, over);
        assert!(!merged.use_aliases);
        assert_eq!(merged.max_suggestions, 8);
        assert_eq!(merged.ui, UiMode::Popup);
    }
}
