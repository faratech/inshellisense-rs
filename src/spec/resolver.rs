//! Spec resolver — Rust port of inshellisense's runSubcommand/runArg/runOption
//! from /tmp/inshellisense/src/runtime/runtime.ts:235-406.
//!
//! Given a tokenized command line + a root Spec, walks the tree to find the
//! "active context": which subcommand the user is inside, which arg they're
//! currently filling, and which persistent options are in scope.
//!
//! The result is a `ResolveResult` the suggest engine turns into a
//! `Vec<Suggestion>`.

use super::Registry;
use super::model::{Arg, LoadSpec, Opt, Subcommand};
use super::parser::CommandToken;
use std::sync::LazyLock;

/// A shared empty spec used to signal "offer no suggestions" — e.g. a
/// `requiresSeparator` option typed without its `=value`. Resolving to a
/// spec with no subcommands/options/args yields an empty suggestion list.
static EMPTY_SUBCOMMAND: LazyLock<Subcommand> = LazyLock::new(Subcommand::default);

#[derive(Debug, Clone)]
pub struct ResolveResult<'a> {
    pub subcommand: &'a Subcommand,
    /// The arg the user is currently typing into, if any.
    pub active_arg: Option<&'a Arg>,
    /// All options currently in scope (subcommand + ancestors' persistent).
    pub persistent_options: Vec<&'a Opt>,
    /// True when no more positional args remain and we should fall back to
    /// subcommand-driven suggestions (option names, subcommand names).
    pub args_depleted: bool,
    /// The partial token the user is currently typing (if incomplete).
    pub active_partial: Option<&'a CommandToken>,
    /// True when we got here via an option consuming its argument.
    pub from_option: bool,
    /// Option tokens the user has already typed. The suggest engine uses
    /// this to filter `exclusive_on` / `depends_on` candidates.
    pub accepted_option_tokens: Vec<String>,
    /// True once this subcommand has consumed a positional argument.
    pub positional_args_consumed: bool,
}

/// Resolve the token stream against the spec. `tokens` should be the output
/// of `parser::parse_command` — the first token is the command name itself.
///
/// No registry variant: `load_spec: SpecPath { name }` substitutions are
/// disabled. Call `resolve_with_registry` to enable them.
pub fn resolve<'a>(root: &'a Subcommand, tokens: &'a [CommandToken]) -> ResolveResult<'a> {
    let rest = if tokens.is_empty() {
        &[][..]
    } else {
        &tokens[1..]
    };
    let ctx = Ctx {
        persistent: Vec::new(),
        accepted_options: Vec::new(),
        positional_args_consumed: false,
        registry: None,
    };
    run_subcommand(rest, root, ctx, false, false)
}

/// Resolve with a registry reference — enables `LoadSpec::SpecPath` lookups
/// when a subcommand declares `load_spec` as a name reference (e.g.
/// `aws/ec2`). The target spec must already be in the registry.
pub fn resolve_with_registry<'a>(
    registry: &'a Registry,
    root: &'a Subcommand,
    tokens: &'a [CommandToken],
) -> ResolveResult<'a> {
    let rest = if tokens.is_empty() {
        &[][..]
    } else {
        &tokens[1..]
    };
    let ctx = Ctx {
        persistent: Vec::new(),
        accepted_options: Vec::new(),
        positional_args_consumed: false,
        registry: Some(registry),
    };
    run_subcommand(rest, root, ctx, false, false)
}

#[derive(Clone)]
struct Ctx<'a> {
    persistent: Vec<&'a Opt>,
    accepted_options: Vec<String>,
    positional_args_consumed: bool,
    registry: Option<&'a Registry>,
}

/// If `sub` has a `load_spec: SpecPath`, resolve it against the registry and
/// return the loaded spec; otherwise return `sub` unchanged.
fn maybe_substitute<'a>(sub: &'a Subcommand, ctx: &Ctx<'a>) -> &'a Subcommand {
    if let (Some(LoadSpec::SpecPath { name }), Some(reg)) = (&sub.load_spec, ctx.registry) {
        if let Some(loaded) = reg.get(name) {
            return loaded;
        }
    }
    sub
}

fn run_subcommand<'a>(
    tokens: &'a [CommandToken],
    sub: &'a Subcommand,
    ctx: Ctx<'a>,
    args_depleted: bool,
    _args_used: bool,
) -> ResolveResult<'a> {
    // Base case: no tokens left — suggest at this subcommand's level.
    if tokens.is_empty() {
        return ResolveResult {
            subcommand: sub,
            active_arg: sub.args.first(),
            persistent_options: merged_options(&ctx.persistent, &sub.options),
            args_depleted,
            active_partial: None,
            from_option: false,
            accepted_option_tokens: ctx.accepted_options.clone(),
            positional_args_consumed: ctx.positional_args_consumed,
        };
    }

    // If the first token is incomplete, that's the partial the user is typing.
    if !tokens[0].complete {
        return ResolveResult {
            subcommand: sub,
            active_arg: sub.args.first(),
            persistent_options: merged_options(&ctx.persistent, &sub.options),
            args_depleted,
            active_partial: Some(&tokens[0]),
            from_option: false,
            accepted_option_tokens: ctx.accepted_options.clone(),
            positional_args_consumed: ctx.positional_args_consumed,
        };
    }

    let active = &tokens[0];
    let all_opts: Vec<&Opt> = ctx
        .persistent
        .iter()
        .copied()
        .chain(sub.options.iter())
        .collect();

    // Option?
    if active.is_option && !active.is_raw {
        if let Some(opt) = find_option(&all_opts, &active.token) {
            let mut new_ctx = ctx.clone();
            new_ctx.accepted_options.push(active.token.clone());
            return run_option(tokens, opt, sub, new_ctx);
        }
        // Unknown option — fall through but don't match as subcommand.
        return ResolveResult {
            subcommand: sub,
            active_arg: None,
            persistent_options: merged_options(&ctx.persistent, &sub.options),
            args_depleted,
            active_partial: None,
            from_option: false,
            accepted_option_tokens: ctx.accepted_options.clone(),
            positional_args_consumed: ctx.positional_args_consumed,
        };
    }

    // Raw tokens (after `--`) only match positional args.
    if !active.is_raw {
        // Subcommand?
        if let Some(next) = sub.subcommands.iter().find(|s| s.matches(&active.token)) {
            let mut new_ctx = ctx.clone();
            for opt in &sub.options {
                if opt.is_persistent && !new_ctx.persistent.iter().any(|o| opts_eq(o, opt)) {
                    new_ctx.persistent.push(opt);
                }
            }
            // LoadSpec::SpecPath substitution: if the matched subcommand is
            // just a stub pointing to another spec, load it lazily.
            let resolved = maybe_substitute(next, &new_ctx);
            return run_subcommand(&tokens[1..], resolved, new_ctx, false, false);
        }
    }

    // Positional arg?
    if !sub.args.is_empty() {
        return run_arg(tokens, &sub.args, sub, ctx, false, false);
    }

    // Nothing matched — skip the token and keep going at this level.
    run_subcommand(&tokens[1..], sub, ctx, args_depleted, true)
}

fn run_option<'a>(
    tokens: &'a [CommandToken],
    opt: &'a Opt,
    sub: &'a Subcommand,
    ctx: Ctx<'a>,
) -> ResolveResult<'a> {
    // If the option takes args, the next token(s) feed them.
    if !opt.args.is_empty() {
        // `requiresSeparator` option supplied without its attached value
        // (`-i ` rather than `-i=val`): upstream suppresses all suggestions.
        // Our tokenizer can't see the `=`, but an option that is the last
        // token (no following value token) means no value was given.
        if opt.requires_separator.is_some() && tokens.len() <= 1 {
            return ResolveResult {
                subcommand: &EMPTY_SUBCOMMAND,
                active_arg: None,
                persistent_options: Vec::new(),
                args_depleted: true,
                active_partial: None,
                from_option: true,
                accepted_option_tokens: ctx.accepted_options.clone(),
                positional_args_consumed: ctx.positional_args_consumed,
            };
        }
        return run_arg(&tokens[1..], &opt.args, sub, ctx, true, false);
    }
    // No args — consume the option token and continue at subcommand level.
    run_subcommand(&tokens[1..], sub, ctx, false, false)
}

fn run_arg<'a>(
    tokens: &'a [CommandToken],
    args: &'a [Arg],
    sub: &'a Subcommand,
    ctx: Ctx<'a>,
    from_option: bool,
    _from_variadic: bool,
) -> ResolveResult<'a> {
    if args.is_empty() {
        return run_subcommand(tokens, sub, ctx, true, !from_option);
    }

    // No tokens left → suggest for the first remaining arg.
    if tokens.is_empty() {
        return ResolveResult {
            subcommand: sub,
            active_arg: Some(&args[0]),
            persistent_options: merged_options(&ctx.persistent, &sub.options),
            args_depleted: false,
            active_partial: None,
            from_option,
            accepted_option_tokens: ctx.accepted_options.clone(),
            positional_args_consumed: ctx.positional_args_consumed,
        };
    }

    // First token incomplete → active partial.
    if !tokens[0].complete {
        return ResolveResult {
            subcommand: sub,
            active_arg: Some(&args[0]),
            persistent_options: merged_options(&ctx.persistent, &sub.options),
            args_depleted: false,
            active_partial: Some(&tokens[0]),
            from_option,
            accepted_option_tokens: ctx.accepted_options.clone(),
            positional_args_consumed: ctx.positional_args_consumed,
        };
    }

    let active = &tokens[0];
    // If all remaining args are optional, the user may be skipping to an
    // option or subcommand instead.
    if args.iter().all(|a| a.is_optional) {
        let all_opts: Vec<&Opt> = ctx
            .persistent
            .iter()
            .copied()
            .chain(sub.options.iter())
            .collect();
        if active.is_option && !active.is_raw {
            if let Some(opt) = find_option(&all_opts, &active.token) {
                let mut new_ctx = ctx.clone();
                new_ctx.accepted_options.push(active.token.clone());
                return run_option(tokens, opt, sub, new_ctx);
            }
            return ResolveResult {
                subcommand: sub,
                active_arg: Some(&args[0]),
                persistent_options: merged_options(&ctx.persistent, &sub.options),
                args_depleted: false,
                active_partial: None,
                from_option,
                accepted_option_tokens: ctx.accepted_options.clone(),
                positional_args_consumed: ctx.positional_args_consumed,
            };
        }
        if !active.is_raw {
            if let Some(next) = sub.subcommands.iter().find(|s| s.matches(&active.token)) {
                let resolved = maybe_substitute(next, &ctx);
                return run_subcommand(&tokens[1..], resolved, ctx, false, false);
            }
        }
    }

    let active_arg = &args[0];

    // `isCommand` arg: this token names another command (e.g. `sudo ls`,
    // `time ls`, `env ls`, `strace ls`). Load that command's spec from the
    // registry and resolve the remaining tokens against it, in a fresh
    // context. Mirrors the `load_spec` substitution path above.
    if active_arg.is_command && !active.is_raw {
        if let Some(reg) = ctx.registry {
            if let Some(loaded) = reg.get(&active.token) {
                let nested = Ctx {
                    persistent: Vec::new(),
                    accepted_options: Vec::new(),
                    positional_args_consumed: false,
                    registry: ctx.registry,
                };
                return run_subcommand(&tokens[1..], loaded, nested, false, false);
            }
        }
    }

    if active_arg.is_variadic {
        let mut new_ctx = ctx;
        if !from_option {
            new_ctx.positional_args_consumed = true;
        }
        return run_arg(&tokens[1..], args, sub, new_ctx, from_option, true);
    }

    // Move to the next positional arg definition.
    let mut new_ctx = ctx;
    if !from_option {
        new_ctx.positional_args_consumed = true;
    }
    run_arg(&tokens[1..], &args[1..], sub, new_ctx, from_option, false)
}

fn find_option<'a>(opts: &[&'a Opt], token: &str) -> Option<&'a Opt> {
    opts.iter().copied().find(|o| o.matches(token))
}

fn opts_eq(a: &Opt, b: &Opt) -> bool {
    a.names == b.names
}

fn merged_options<'a>(persistent: &[&'a Opt], local: &'a [Opt]) -> Vec<&'a Opt> {
    let mut out: Vec<&Opt> = persistent.to_vec();
    for o in local {
        if !out.iter().any(|p| opts_eq(p, o)) {
            out.push(o);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::model::*;

    fn make_spec(name: &str, subs: Vec<Subcommand>, opts: Vec<Opt>) -> Subcommand {
        Subcommand {
            names: vec![name.to_string()],
            subcommands: subs,
            options: opts,
            ..Default::default()
        }
    }

    fn make_opt(name: &str) -> Opt {
        Opt {
            names: vec![name.to_string()],
            ..Default::default()
        }
    }

    fn make_arg() -> Arg {
        Arg::default()
    }

    fn tok(s: &str) -> Vec<CommandToken> {
        crate::spec::parse_command(s)
    }

    #[test]
    fn resolve_simple_subcommand() {
        let root = make_spec(
            "git",
            vec![
                make_spec("status", vec![], vec![]),
                make_spec("commit", vec![], vec![]),
            ],
            vec![],
        );
        let tokens = tok("git status ");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "status");
    }

    #[test]
    fn resolve_partial_subcommand() {
        let root = make_spec("git", vec![make_spec("status", vec![], vec![])], vec![]);
        let tokens = tok("git sta");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "git");
        assert_eq!(result.active_partial.unwrap().token, "sta");
    }

    #[test]
    fn resolve_option_tracking() {
        let root = make_spec(
            "git",
            vec![make_spec("status", vec![], vec![make_opt("--short")])],
            vec![],
        );
        let tokens = tok("git status --short ");
        let result = resolve(&root, &tokens);
        assert!(
            result
                .accepted_option_tokens
                .contains(&"--short".to_string())
        );
    }

    #[test]
    fn resolve_double_dash_raw() {
        let root = make_spec("git", vec![make_spec("log", vec![], vec![])], vec![]);
        let tokens = tok("git log -- file1");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "log");
    }

    #[test]
    fn resolve_unknown_subcommand_stays_at_root() {
        let root = make_spec("git", vec![make_spec("status", vec![], vec![])], vec![]);
        let tokens = tok("git nonexistent ");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "git");
    }

    #[test]
    fn resolve_nested_subcommand() {
        let inner = make_spec("remote", vec![make_spec("add", vec![], vec![])], vec![]);
        let root = make_spec("git", vec![inner], vec![]);
        let tokens = tok("git remote add ");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "add");
    }

    #[test]
    fn resolve_with_arg() {
        let mut sub = make_spec("commit", vec![], vec![make_opt("-m")]);
        sub.args = vec![make_arg()];
        let root = make_spec("git", vec![sub], vec![]);
        let tokens = tok("git commit -m ");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "commit");
    }

    #[test]
    fn resolve_persistent_option() {
        let mut root = make_spec(
            "git",
            vec![make_spec("status", vec![], vec![])],
            vec![make_opt("-C")],
        );
        root.options[0].is_persistent = true;
        let tokens = tok("git status ");
        let result = resolve(&root, &tokens);
        assert!(
            result
                .persistent_options
                .iter()
                .any(|o| o.names.contains(&"-C".to_string()))
        );
    }
}
