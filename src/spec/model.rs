//! Fig spec data model — Rust mirror of `@withfig/autocomplete-types`.
//!
//! Kept deliberately close to the upstream TypeScript schema so an extractor
//! can deserialize real Fig specs directly into these structs. The phase-1
//! surface covers everything inshellisense's runtime actually consumes plus
//! the HIGH-priority fields from the plan.

use serde::Deserialize;

/// Root spec form — a command like `git` resolves to one of these.
pub type Spec = Subcommand;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Subcommand {
    /// Primary name plus any aliases. Always at least one entry.
    #[serde(deserialize_with = "de_string_or_vec")]
    pub names: Vec<String>,

    pub display_name: Option<String>,
    pub description: Option<String>,

    pub subcommands: Vec<Subcommand>,
    pub options: Vec<Opt>,
    #[serde(deserialize_with = "de_arg_or_vec", default)]
    pub args: Vec<Arg>,

    pub load_spec: Option<LoadSpec>,

    pub filter_strategy: FilterStrategy,
    pub requires_subcommand: bool,
    pub parser_directives: ParserDirectives,

    pub priority: Option<u8>,
    pub hidden: bool,
    pub is_dangerous: bool,
    pub deprecated: bool,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Opt {
    #[serde(deserialize_with = "de_string_or_vec")]
    pub names: Vec<String>,

    pub display_name: Option<String>,
    pub description: Option<String>,

    #[serde(deserialize_with = "de_arg_or_vec", default)]
    pub args: Vec<Arg>,

    pub is_persistent: bool,
    pub is_required: bool,
    pub is_repeatable: Repeatable,
    pub requires_separator: Option<String>,
    pub exclusive_on: Vec<String>,
    pub depends_on: Vec<String>,

    pub priority: Option<u8>,
    pub hidden: bool,
    pub deprecated: bool,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Arg {
    pub name: Option<String>,
    pub description: Option<String>,

    /// Inline static suggestions with metadata.
    pub suggestions: Vec<Suggestion>,
    /// Dynamic suggestion sources. Upstream Fig allows either a single
    /// generator or an array — we always store a Vec.
    pub generators: Vec<Generator>,
    /// Built-in templates.
    pub templates: Vec<Template>,

    pub is_variadic: bool,
    pub is_optional: bool,
    pub is_command: bool,
    pub is_script: bool,
    pub debounce: bool,
    pub options_can_break_variadic_arg: bool,

    pub filter_strategy: FilterStrategy,
    pub default: Option<String>,
    pub load_spec: Option<LoadSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Generator {
    /// Run a shell script; optionally split and post-process output.
    Script {
        input: ScriptInput,
        #[serde(default)]
        split_on: Option<String>,
        #[serde(default)]
        post_process: PostProcess,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u32,
        #[serde(default)]
        cache: Option<CacheSpec>,
    },
    /// Opaque JS generator; resolved via the js_bridge in phase 6.
    Custom { fn_id: u32 },
    /// A built-in template (filepaths, folders, history, help).
    Template { template: Template },
    /// Shell glob expansion.
    Glob { pattern: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScriptInput {
    /// A raw shell command string — run with `sh -c`.
    Shell { script: String },
    /// Argv form — run without a shell.
    Argv { argv: Vec<String> },
    /// A templated form whose placeholders are filled from the token stream.
    /// `{tokens[2]}` etc. Phase 4 pattern-matches these from TS closures.
    FnTemplate { template: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PostProcess {
    /// No post-processing; values come from `split_on` directly.
    None {},
    /// Split on the given separator (overrides generator.split_on).
    Split { sep: String },
    /// A named structural pattern the extractor recognized.
    Pattern { inner: PostProcessKind },
    /// Opaque JS callback; resolved via js_bridge.
    Fn { fn_id: u32 },
}

impl Default for PostProcess {
    fn default() -> Self {
        PostProcess::None {}
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PostProcessKind {
    SplitLines {},
    SplitLinesFiltered { skip_prefixes: Vec<String> },
    JsonParse {},
    JsonPath { path: String },
    GitBranches {},
    KeyValueColon {},
    TableColumn { index: u8, sep: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LoadSpec {
    /// Look up another spec by name — resolved lazily by the Registry.
    SpecPath { name: String },
    /// Inline subcommand body.
    Inline { spec: Box<Subcommand> },
    /// Opaque JS function — resolved via js_bridge.
    Function { fn_id: u32 },
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Template {
    Filepaths,
    Folders,
    History,
    Help,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterStrategy {
    #[default]
    Default, // prefix match, case-insensitive
    Prefix,
    Fuzzy,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(untagged)]
pub enum Repeatable {
    #[default]
    False,
    True,
    N(u16),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ParserDirectives {
    pub flags_are_posix_noncompliant: bool,
    pub options_must_precede_arguments: bool,
    pub option_arg_separators: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheSpec {
    pub strategy: CacheStrategy,
    pub ttl_secs: u64,
    pub by_dir: bool,
}

impl Default for CacheSpec {
    fn default() -> Self {
        Self {
            strategy: CacheStrategy::MaxAge,
            ttl_secs: 30,
            by_dir: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheStrategy {
    #[default]
    MaxAge,
    StaleWhileRevalidate,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Suggestion {
    pub name: String,
    #[serde(default)]
    pub all_names: Vec<String>,
    pub display_name: Option<String>,
    pub insert_value: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub suggestion_type: SuggestionType,
    pub priority: Option<u8>,
    pub is_dangerous: bool,
    pub hidden: bool,
    pub deprecated: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionType {
    #[default]
    Arg,
    Folder,
    File,
    Subcommand,
    Option,
    Special,
    Mixin,
    Shortcut,
}

fn default_timeout_ms() -> u32 {
    5000
}

// ---- custom deserializers for string | string[] + arg | arg[] upstream forms ----

fn de_string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        One(String),
        Many(Vec<String>),
    }
    match V::deserialize(d) {
        Ok(V::One(s)) => Ok(vec![s]),
        Ok(V::Many(v)) => Ok(v),
        Err(e) => Err(Error::custom(e.to_string())),
    }
}

fn de_arg_or_vec<'de, D>(d: D) -> Result<Vec<Arg>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        One(Arg),
        Many(Vec<Arg>),
    }
    match V::deserialize(d) {
        Ok(V::One(a)) => Ok(vec![a]),
        Ok(V::Many(v)) => Ok(v),
        Err(e) => Err(Error::custom(e.to_string())),
    }
}

// ---- ergonomic constructors used by curated specs ----

impl Subcommand {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            names: vec![name.into()],
            ..Default::default()
        }
    }
    pub fn name(&self) -> &str {
        self.names.first().map(|s| s.as_str()).unwrap_or("")
    }
    pub fn matches(&self, token: &str) -> bool {
        self.names.iter().any(|n| n == token)
    }
}

impl Opt {
    pub fn new(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
    pub fn matches(&self, token: &str) -> bool {
        self.names.iter().any(|n| n == token)
    }
}
