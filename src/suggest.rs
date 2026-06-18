//! Suggestion engine — maps a command line + cwd to either a tail-string
//! ghost suggestion (current renderer's interface) or a structured
//! `Vec<Suggestion>` (phase 2 consumers).
//!
//! Phase 1 keeps the old `suggest(line, cwd) -> Option<String>` method so
//! the PTY renderer keeps working. Phase 2 introduces `suggest_blob()`
//! returning `Vec<Suggestion>` for inspection via `is complete`.

use crate::generator;
use crate::history;
use crate::shell::Shell;
use crate::spec::{
    self, Registry,
    filter::matches,
    model::{Arg, FilterStrategy, Opt, Subcommand, Suggestion, SuggestionType},
    parser::CommandToken,
    resolver::{self, ResolveResult},
};
use std::collections::HashMap;

pub struct Engine {
    registry: Registry,
    history: Vec<String>,
    aliases: HashMap<String, String>,
    /// True for offline `complete` queries. Upstream's offline `complete`
    /// has no live shell-session history, so the `history` template must
    /// yield nothing here (otherwise `curl ` returns `exit, ls, cd ..`).
    /// The interactive PTY engine leaves this false.
    offline: bool,
    /// Active shell context. Used for shell-specific aliases/builtins such as
    /// PowerShell's `ls` alias for `Get-ChildItem`.
    shell: Option<Shell>,
}

impl Engine {
    pub fn new(registry: Registry, history: Vec<String>) -> Self {
        Self {
            registry,
            history,
            aliases: HashMap::new(),
            offline: false,
            shell: None,
        }
    }

    pub fn set_aliases(&mut self, aliases: HashMap<String, String>) {
        self.aliases = aliases;
    }

    /// Mark this engine as serving offline `complete` queries (suppresses
    /// the shell-history template, matching upstream).
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    pub fn set_shell(&mut self, shell: Shell) {
        self.shell = Some(shell);
    }

    /// Ghost-text tail: the portion of the top suggestion after the current
    /// partial token.
    pub fn suggest(&self, line: &str, cwd: &str) -> Option<String> {
        if line.trim().is_empty() {
            return None;
        }
        let blob = self.suggest_blob(line, cwd);
        let partial = current_partial(line);
        if let Some(top) = blob.first() {
            if top.name.len() > partial.len() && top.name.starts_with(&partial) {
                return Some(top.name[partial.len()..].to_string());
            }
        }
        // Fallback to history.
        if !self.offline
            && let Some(full) = history::best_match(&self.history, line)
        {
            return Some(full[line.len()..].to_string());
        }
        None
    }

    /// Full suggestion blob — sorted and filtered.
    pub fn suggest_blob(&self, line: &str, cwd: &str) -> Vec<Suggestion> {
        let original_tokens = spec::parse_command(line);
        if original_tokens.is_empty() {
            return Vec::new();
        }

        // First-word completion — when the user has typed exactly one
        // token with no trailing space, they are *still typing the
        // command name*. Include both spec names and alias names,
        // with aliases at priority 100 (upstream: runtime.ts:412-426).
        let original_trailing_space = line.ends_with(char::is_whitespace);
        if original_tokens.len() == 1 && !original_trailing_space {
            let cmd = &original_tokens[0].token;
            let mut results = self.top_level_name_matches(cmd);
            // Add matching aliases (priority 100, type Shortcut).
            let partial_lc = cmd.to_lowercase();
            for (name, value) in &self.aliases {
                if name.to_lowercase().starts_with(&partial_lc) {
                    results.push(Suggestion {
                        name: name.clone(),
                        description: Some(value.clone()),
                        suggestion_type: SuggestionType::Shortcut,
                        priority: Some(100),
                        ..Default::default()
                    });
                }
            }
            results.sort_by(|a, b| {
                b.priority
                    .unwrap_or(50)
                    .cmp(&a.priority.unwrap_or(50))
                    .then_with(|| (b.name == *cmd).cmp(&(a.name == *cmd)))
                    .then_with(|| a.name.len().cmp(&b.name.len()))
            });
            return dedup_by_name(results);
        }

        // Expand aliases before resolving — if the first word in the active
        // command segment is an alias, suggestions should reflect the expanded
        // command (upstream: runtime.ts:110).
        let expanded = crate::alias::expand_active_segment(line, &self.aliases);
        let tokens = spec::parse_command(&expanded);
        if tokens.is_empty() {
            return Vec::new();
        }
        let cmd = &tokens[0].token;

        let shell_root = shell_command_override(self.shell, cmd);
        let Some(root) = shell_root.as_ref().or_else(|| self.registry.get(cmd)) else {
            return Vec::new();
        };
        let result = resolver::resolve_with_registry(&self.registry, root, &tokens);

        let partial = result
            .active_partial
            .map(|t| t.token.clone())
            .unwrap_or_default();

        let mut candidates: Vec<Suggestion> = Vec::new();

        // Trailing space or partial that doesn't start with '-'? Offer
        // subcommand names.
        let trailing_space = line.ends_with(char::is_whitespace);
        let completing_option = !trailing_space
            && (partial.starts_with('-') || partial_matches_option(&result, &partial));
        let completing_name = !trailing_space && !completing_option;

        if trailing_space || completing_name {
            for s in &result.subcommand.subcommands {
                if s.hidden {
                    continue;
                }
                // Upstream picks the longest name when there's no
                // partial, and the first matching name otherwise
                // (from runtime/suggestion.ts::filter). Match exactly.
                let Some(name) = pick_primary(&s.names, &partial) else {
                    continue;
                };
                candidates.push(Suggestion {
                    name,
                    all_names: s.names.clone(),
                    description: s.description.clone(),
                    suggestion_type: SuggestionType::Subcommand,
                    priority: Some(s.priority.unwrap_or(50)),
                    ..Default::default()
                });
            }
        }

        if trailing_space || completing_option {
            for opt in &result.persistent_options {
                if opt.hidden {
                    continue;
                }
                // Respect exclusive_on: if any of this option's names has
                // already been used, or if any accepted option is listed in
                // this option's exclusive_on set, suppress it.
                let already_used = opt
                    .names
                    .iter()
                    .any(|n| result.accepted_option_tokens.iter().any(|a| a == n));
                use crate::spec::model::Repeatable;
                let is_rep = matches!(opt.is_repeatable, Repeatable::Bool(true) | Repeatable::N(_));
                if already_used && !is_rep {
                    continue;
                }
                let excluded = opt
                    .exclusive_on
                    .iter()
                    .any(|e| result.accepted_option_tokens.iter().any(|a| a == e));
                if excluded {
                    continue;
                }
                let dependency_missing = !opt.depends_on.is_empty()
                    && !opt
                        .depends_on
                        .iter()
                        .any(|d| result.accepted_option_tokens.iter().any(|a| a == d));
                if dependency_missing {
                    continue;
                }
                if result.positional_args_consumed
                    && result
                        .subcommand
                        .parser_directives
                        .options_must_precede_arguments
                {
                    continue;
                }
                let Some(name) = pick_primary(&opt.names, &partial) else {
                    continue;
                };
                candidates.push(Suggestion {
                    name,
                    all_names: opt.names.clone(),
                    description: opt.description.clone(),
                    suggestion_type: SuggestionType::Option,
                    priority: Some(opt.priority.unwrap_or(45)),
                    ..Default::default()
                });
            }
        }

        // Arg-driven suggestions.
        if let Some(arg) = result.active_arg {
            candidates.extend(generator::suggestions_for_arg(
                arg,
                cwd,
                &partial,
                !self.offline,
            ));
        }

        // Filter by partial, using each candidate's effective filter strategy.
        let strategy = active_filter_strategy(&result);
        candidates.retain(|c| {
            if c.name.is_empty() {
                return false;
            }
            // File/folder suggestions match by case-insensitive SUBSTRING on
            // the basename — same as the generator and upstream (`ls fil` →
            // `afile.txt`, `ls -` → `specs-data`). This also correctly drops a
            // path entry like `..` when the user is typing an option (`--ve`),
            // and tolerates a dir prefix (`sub/fi` → `sub/file`).
            if matches!(
                c.suggestion_type,
                SuggestionType::Folder | SuggestionType::File
            ) {
                let base = c.name.rsplit('/').next().unwrap_or(c.name.as_str());
                let needle = partial.rsplit('/').next().unwrap_or(partial.as_str());
                return base.to_lowercase().contains(&needle.to_lowercase());
            }
            matches(strategy, &c.name, &partial)
        });

        // Stable sort by EXACT-case prefix matches first, then priority DESC.
        // Both `-L` and `-l` still appear (case-insensitive filter, like
        // upstream), but typing `-l` ranks `-l` above even a higher-priority
        // `-L` so the exact case the user typed is the active/ghost selection
        // (GH issue #2). Within a tier we otherwise preserve insertion order
        // (spec-authored order for subcommands/options, alphabetical for
        // filepaths).
        candidates.sort_by(|a, b| {
            let pa = a.priority.unwrap_or(50);
            let pb = b.priority.unwrap_or(50);
            let ea = a.name.starts_with(&partial);
            let eb = b.name.starts_with(&partial);
            eb.cmp(&ea).then_with(|| pb.cmp(&pa))
        });

        dedup_by_name(candidates)
    }

    /// Top-level command-name completion — returns every registered
    /// command whose primary name starts with `partial`. Used when
    /// the user is typing the first word of a command line and hasn't
    /// finished the command name yet.
    fn top_level_name_matches(&self, partial: &str) -> Vec<Suggestion> {
        if partial.is_empty() {
            return Vec::new();
        }
        let partial_lc = partial.to_lowercase();
        let mut out: Vec<Suggestion> = Vec::new();
        for name in self.registry.names() {
            // Skip nested path-keys like `aws/ec2` — those are
            // sub-spec load targets, not top-level executables.
            if name.contains('/') {
                continue;
            }
            if !name.to_lowercase().starts_with(&partial_lc) {
                continue;
            }
            let spec = match self.registry.get(&name) {
                Some(s) => s,
                None => continue,
            };
            out.push(Suggestion {
                name: name.to_string(),
                all_names: spec.names.clone(),
                description: spec.description.clone(),
                suggestion_type: SuggestionType::Subcommand,
                priority: Some(spec.priority.unwrap_or(50)),
                icon: spec.icon.clone(),
                ..Default::default()
            });
        }
        // Sort alphabetically — upstream shows suggestions in
        // registry-insertion order which is effectively alpha because
        // @withfig/autocomplete names specs alphabetically. Matching
        // this order keeps the popup pixel-identical to upstream for
        // top-level command completion.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

fn active_filter_strategy(r: &ResolveResult<'_>) -> FilterStrategy {
    if let Some(arg) = r.active_arg {
        if arg.filter_strategy != FilterStrategy::Default {
            return arg.filter_strategy;
        }
    }
    r.subcommand.filter_strategy
}

fn partial_matches_option(result: &ResolveResult<'_>, partial: &str) -> bool {
    if partial.is_empty() {
        return false;
    }
    let needle = partial.to_lowercase();
    result.persistent_options.iter().any(|opt| {
        opt.names
            .iter()
            .any(|name| name.to_lowercase().starts_with(&needle))
    })
}

fn shell_command_override(shell: Option<Shell>, cmd: &str) -> Option<Subcommand> {
    match shell {
        Some(Shell::Pwsh | Shell::Powershell) if is_powershell_get_child_item(cmd) => {
            Some(powershell_get_child_item_spec())
        }
        #[cfg(windows)]
        Some(Shell::Cmd) if is_cmd_dir(cmd) => Some(cmd_dir_spec()),
        _ => None,
    }
}

fn is_powershell_get_child_item(cmd: &str) -> bool {
    matches!(
        cmd.to_ascii_lowercase().as_str(),
        "get-childitem" | "ls" | "dir" | "gci"
    )
}

fn powershell_get_child_item_spec() -> Subcommand {
    let mut spec = Subcommand::new("Get-ChildItem");
    spec.names = vec![
        "Get-ChildItem".into(),
        "ls".into(),
        "dir".into(),
        "gci".into(),
    ];
    spec.description = Some("Gets the items and child items in one or more locations".into());
    spec.options = vec![
        ps_opt(&["-Path"], "Specifies one or more paths"),
        ps_opt(&["-LiteralPath"], "Uses the path exactly as typed"),
        ps_opt(&["-Filter"], "Specifies a provider filter"),
        ps_opt(&["-Include"], "Includes only matching items"),
        ps_opt(&["-Exclude"], "Omits matching items"),
        ps_opt(&["-Recurse"], "Gets items in child locations"),
        ps_opt(&["-Depth"], "Limits recursive depth"),
        ps_opt(&["-Force"], "Gets hidden or system items"),
        ps_opt(&["-Name"], "Returns only item names"),
        ps_opt(
            &["-Attributes"],
            "Gets files and folders with specified attributes",
        ),
        ps_opt(&["-Directory"], "Gets only directories"),
        ps_opt(&["-File"], "Gets only files"),
        ps_opt(&["-Hidden"], "Gets only hidden items"),
        ps_opt(&["-ReadOnly"], "Gets only read-only items"),
        ps_opt(&["-System"], "Gets only system items"),
        ps_opt(&["-ErrorAction"], "Specifies the action for errors"),
        ps_opt(&["-Verbose"], "Displays detailed operation information"),
        ps_opt(&["-Debug"], "Displays debug information"),
        ps_opt(&["-WhatIf"], "Shows what would happen without running"),
        ps_opt(&["-Confirm"], "Prompts before running"),
    ];
    spec.args = vec![Arg {
        name: Some("path".into()),
        is_optional: true,
        is_variadic: true,
        ..Default::default()
    }];
    spec
}

fn ps_opt(names: &[&str], description: &str) -> Opt {
    Opt {
        names: names.iter().map(|s| s.to_string()).collect(),
        description: Some(description.to_string()),
        priority: Some(60),
        ..Default::default()
    }
}

#[cfg(windows)]
fn is_cmd_dir(cmd: &str) -> bool {
    cmd.eq_ignore_ascii_case("dir")
}

#[cfg(windows)]
fn cmd_dir_spec() -> Subcommand {
    let mut spec = Subcommand::new("dir");
    spec.description = Some("Displays a list of files and subdirectories in a directory".into());
    spec.options = vec![
        cmd_opt(&["/A"], "Displays files with specified attributes"),
        cmd_opt(&["/B"], "Uses bare format"),
        cmd_opt(&["/C"], "Displays thousands separator in file sizes"),
        cmd_opt(&["/D"], "Uses wide list format sorted by column"),
        cmd_opt(&["/L"], "Uses lowercase"),
        cmd_opt(&["/N"], "Uses long list format"),
        cmd_opt(&["/O"], "Lists files in sorted order"),
        cmd_opt(&["/P"], "Pauses after each screen"),
        cmd_opt(&["/Q"], "Displays file ownership"),
        cmd_opt(&["/R"], "Displays alternate data streams"),
        cmd_opt(
            &["/S"],
            "Lists matching files in current and subdirectories",
        ),
        cmd_opt(&["/T"], "Controls which time field is displayed"),
        cmd_opt(&["/W"], "Uses wide list format"),
        cmd_opt(&["/X"], "Displays short names"),
        cmd_opt(&["/4"], "Displays four-digit years"),
        cmd_opt(&["/?"], "Displays help"),
    ];
    spec.args = vec![Arg {
        name: Some("path".into()),
        is_optional: true,
        is_variadic: true,
        ..Default::default()
    }];
    spec
}

#[cfg(windows)]
fn cmd_opt(names: &[&str], description: &str) -> Opt {
    Opt {
        names: names.iter().map(|s| s.to_string()).collect(),
        description: Some(description.to_string()),
        priority: Some(60),
        ..Default::default()
    }
}

/// Port of upstream's name-picking logic from
/// `/tmp/inshellisense/src/runtime/suggestion.ts::filter`.
///
/// When the user has typed a partial token (e.g. `ls -`), upstream
/// picks the first alias in `names` that starts with the partial —
/// so `[-a, --attach]` with partial `-` picks `-a`. When there's no
/// partial (trailing space case, e.g. `git `), upstream picks the
/// LONGEST name via `getLong` — so `[-p, --paginate]` picks
/// `--paginate`.
///
/// Matching is case-insensitive for all partials, flags included —
/// upstream surfaces both `-C` and `-c` when you type `-c` (verified
/// against the installed upstream binary). Subcommand names like `ch`
/// → `Checkout` keep the same case-insensitive prefix behaviour.
///
/// Returns `None` when names is empty or no alias matches the partial.
fn pick_primary(names: &[String], partial: &str) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    if partial.is_empty() {
        // Longest name wins.
        let longest = names.iter().max_by_key(|n| n.len())?;
        return Some(longest.clone());
    }
    // First name whose case-insensitive prefix matches the partial.
    let p = partial.to_lowercase();
    names
        .iter()
        .find(|n| n.to_lowercase().starts_with(&p))
        .cloned()
}

#[allow(dead_code)]
fn type_rank(t: SuggestionType) -> u8 {
    // Lower is more preferred in ties.
    match t {
        SuggestionType::Subcommand => 0,
        SuggestionType::Option => 1,
        SuggestionType::Arg => 2,
        SuggestionType::Folder => 3,
        SuggestionType::File => 4,
        SuggestionType::Mixin | SuggestionType::Shortcut => 5,
        SuggestionType::Special => 6,
    }
}

fn dedup_by_name(mut v: Vec<Suggestion>) -> Vec<Suggestion> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.name.clone()));
    v
}

fn current_partial(line: &str) -> String {
    if line.ends_with(char::is_whitespace) {
        return String::new();
    }
    // Best-effort: last whitespace-split chunk.
    line.rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_string()
}

// Keep the old tokenize function path alive for any callers that still use it.
#[allow(dead_code)]
pub(crate) fn _pass_through_token(_t: &CommandToken) {}

// Shim: the old `first_arg`-based engine lived in this file; keeping a
// minimal Subcommand ref helper for test plumbing.
#[allow(dead_code)]
fn _first_arg(s: &Subcommand) -> Option<&crate::spec::model::Arg> {
    s.args.first()
}

#[cfg(test)]
mod pick_primary_tests {
    use super::pick_primary;

    #[test]
    fn flag_partial_is_case_insensitive() {
        // Within one option's alias list, the FIRST case-insensitive match
        // wins — matching upstream's first-match `filter` semantics. (In
        // real specs `-C` and `-c` are distinct options, each with a single
        // name, so both surface independently; see filter.rs.)
        let names = vec!["-L".to_string(), "-l".to_string()];
        assert_eq!(pick_primary(&names, "-l").as_deref(), Some("-L"));
        assert_eq!(pick_primary(&names, "-L").as_deref(), Some("-L"));
        let single = vec!["-c".to_string()];
        assert_eq!(pick_primary(&single, "-C").as_deref(), Some("-c"));
    }

    #[test]
    fn empty_partial_picks_longest() {
        let names = vec!["-p".to_string(), "--paginate".to_string()];
        assert_eq!(pick_primary(&names, "").as_deref(), Some("--paginate"));
    }

    #[test]
    fn subcommand_partial_stays_case_insensitive() {
        let names = vec!["Checkout".to_string()];
        assert_eq!(pick_primary(&names, "ch").as_deref(), Some("Checkout"));
    }
}
