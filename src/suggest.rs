//! Suggestion engine — maps a command line + cwd to either a tail-string
//! ghost suggestion (current renderer's interface) or a structured
//! `Vec<Suggestion>` (phase 2 consumers).
//!
//! Phase 1 keeps the old `suggest(line, cwd) -> Option<String>` method so
//! the PTY renderer keeps working. Phase 2 introduces `suggest_blob()`
//! returning `Vec<Suggestion>` for inspection via `insh complete --json`.

use crate::generator;
use crate::history;
use crate::spec::{
    self,
    filter::matches,
    model::{FilterStrategy, Subcommand, Suggestion, SuggestionType},
    parser::CommandToken,
    resolver::{self, ResolveResult},
    Registry,
};

pub struct Engine {
    registry: Registry,
    history: Vec<String>,
}

impl Engine {
    pub fn new(registry: Registry, history: Vec<String>) -> Self {
        Self { registry, history }
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
        if let Some(full) = history::best_match(&self.history, line) {
            return Some(full[line.len()..].to_string());
        }
        None
    }

    /// Full suggestion blob — sorted and filtered.
    pub fn suggest_blob(&self, line: &str, cwd: &str) -> Vec<Suggestion> {
        let tokens = spec::parse_command(line);
        if tokens.is_empty() {
            return Vec::new();
        }
        let cmd = &tokens[0].token;

        // First-word completion — when the user has typed exactly one
        // token with no trailing space, they are *still typing the
        // command name*. Upstream always returns top-level commands
        // starting with that partial, regardless of whether the token
        // is already a fully-formed registered command. e.g. `git`
        // returns `[git, git-cliff, git-flow, git-profile]`, not git's
        // subcommands. Subcommands only appear once the user presses
        // space.
        let trailing_space = line.ends_with(char::is_whitespace);
        if tokens.len() == 1 && !trailing_space {
            return self.top_level_name_matches(cmd);
        }

        let Some(root) = self.registry.get(cmd) else {
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
        let completing_name = !trailing_space && !partial.starts_with('-');
        let completing_option = !trailing_space && partial.starts_with('-');

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
                let Some(name) = pick_primary(&opt.names, &partial) else {
                    continue;
                };
                candidates.push(Suggestion {
                    name,
                    description: opt.description.clone(),
                    suggestion_type: SuggestionType::Option,
                    priority: Some(opt.priority.unwrap_or(45)),
                    ..Default::default()
                });
            }
        }

        // Arg-driven suggestions.
        if let Some(arg) = result.active_arg {
            candidates.extend(generator::suggestions_for_arg(arg, cwd, &partial));
        }

        // Filter by partial, using each candidate's effective filter strategy.
        let strategy = active_filter_strategy(&result);
        candidates.retain(|c| !c.name.is_empty() && matches(strategy, &c.name, &partial));

        // Stable sort by priority DESC only. Within the same priority
        // tier we preserve insertion order, which mirrors upstream's
        // behavior: spec-file authored order for subcommands/options,
        // alphabetical string order for filepaths. Upstream does NOT
        // group folders before files or subcommands before options —
        // ties are broken by whatever order the upstream runtime
        // collected them, which is what our insertion order already
        // replicates.
        candidates.sort_by(|a, b| {
            b.priority.unwrap_or(50).cmp(&a.priority.unwrap_or(50))
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
