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
        let Some(root) = self.registry.get(cmd) else {
            return Vec::new();
        };
        let result = resolver::resolve(root, &tokens);

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
                for n in &s.names {
                    candidates.push(Suggestion {
                        name: n.clone(),
                        description: s.description.clone(),
                        suggestion_type: SuggestionType::Subcommand,
                        priority: Some(s.priority.unwrap_or(50)),
                        ..Default::default()
                    });
                }
            }
        }

        if trailing_space || completing_option {
            for opt in &result.persistent_options {
                if opt.hidden {
                    continue;
                }
                for n in &opt.names {
                    candidates.push(Suggestion {
                        name: n.clone(),
                        description: opt.description.clone(),
                        suggestion_type: SuggestionType::Option,
                        priority: Some(opt.priority.unwrap_or(45)),
                        ..Default::default()
                    });
                }
            }
        }

        // Arg-driven suggestions.
        if let Some(arg) = result.active_arg {
            candidates.extend(generator::suggestions_for_arg(arg, cwd, &partial));
        }

        // Filter by partial, using each candidate's effective filter strategy.
        let strategy = active_filter_strategy(&result);
        candidates.retain(|c| !c.name.is_empty() && matches(strategy, &c.name, &partial));

        // Sort: priority DESC, type precedence, name length ASC.
        candidates.sort_by(|a, b| {
            b.priority
                .unwrap_or(50)
                .cmp(&a.priority.unwrap_or(50))
                .then_with(|| type_rank(a.suggestion_type).cmp(&type_rank(b.suggestion_type)))
                .then_with(|| a.name.len().cmp(&b.name.len()))
        });

        dedup_by_name(candidates)
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
