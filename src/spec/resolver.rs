//! Spec resolver — Rust port of inshellisense's runSubcommand/runArg/runOption
//! from /tmp/inshellisense/src/runtime/runtime.ts:235-406.
//!
//! Given a tokenized command line + a root Spec, walks the tree to find the
//! "active context": which subcommand the user is inside, which arg they're
//! currently filling, and which persistent options are in scope.
//!
//! The result is a `ResolveResult` the suggest engine turns into a
//! `Vec<Suggestion>`.

use super::model::{Arg, LoadSpec, Opt, Subcommand};
use super::parser::CommandToken;
use super::Registry;

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
}

/// Resolve the token stream against the spec. `tokens` should be the output
/// of `parser::parse_command` — the first token is the command name itself.
///
/// No registry variant: `load_spec: SpecPath { name }` substitutions are
/// disabled. Call `resolve_with_registry` to enable them.
pub fn resolve<'a>(root: &'a Subcommand, tokens: &'a [CommandToken]) -> ResolveResult<'a> {
    let rest = if tokens.is_empty() { &[][..] } else { &tokens[1..] };
    let ctx = Ctx {
        persistent: Vec::new(),
        accepted_options: Vec::new(),
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
    let rest = if tokens.is_empty() { &[][..] } else { &tokens[1..] };
    let ctx = Ctx {
        persistent: Vec::new(),
        accepted_options: Vec::new(),
        registry: Some(registry),
    };
    run_subcommand(rest, root, ctx, false, false)
}

#[derive(Clone)]
struct Ctx<'a> {
    persistent: Vec<&'a Opt>,
    accepted_options: Vec<String>,
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
        };
    }

    let active = &tokens[0];
    let all_opts: Vec<&Opt> = ctx.persistent.iter().copied().chain(sub.options.iter()).collect();

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
        };
    }

    let active = &tokens[0];
    // If all remaining args are optional, the user may be skipping to an
    // option or subcommand instead.
    if args.iter().all(|a| a.is_optional) {
        let all_opts: Vec<&Opt> =
            ctx.persistent.iter().copied().chain(sub.options.iter()).collect();
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
    if active_arg.is_variadic {
        return run_arg(&tokens[1..], args, sub, ctx, from_option, true);
    }

    // Move to the next positional arg definition.
    run_arg(&tokens[1..], &args[1..], sub, ctx, from_option, false)
}

fn find_option<'a>(opts: &[&'a Opt], token: &str) -> Option<&'a Opt> {
    opts.iter().copied().find(|o| o.matches(token))
}

fn opts_eq(a: &Opt, b: &Opt) -> bool {
    a.names == b.names
}

fn merged_options<'a>(
    persistent: &[&'a Opt],
    local: &'a [Opt],
) -> Vec<&'a Opt> {
    let mut out: Vec<&Opt> = persistent.to_vec();
    for o in local {
        if !out.iter().any(|p| opts_eq(p, o)) {
            out.push(o);
        }
    }
    out
}
