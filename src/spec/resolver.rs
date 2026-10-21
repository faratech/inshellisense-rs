//! Spec resolver — Rust port of inshellisense's runSubcommand/runArg/runOption
//! from upstream inshellisense's `src/runtime/runtime.ts:235-406`.
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

/// Stand-in returned once a bare `--` has been seen. Everything after `--`
/// is positional-only, so the suggestion surface must expose neither
/// subcommand names nor options. The real subcommand's positional args stay
/// reachable through `active_arg`, which is built from the actual spec
/// before this substitution is applied.
static POSITIONAL_ONLY_SUBCOMMAND: LazyLock<Subcommand> = LazyLock::new(|| Subcommand {
    names: vec![String::new()],
    ..Subcommand::default()
});

/// The spec whose *name* surface the caller may offer. After a bare `--`
/// that is the empty stand-in; otherwise it is `sub` itself.
fn surface<'a>(ctx: &Ctx<'a>, sub: &'a Subcommand) -> &'a Subcommand {
    if ctx.end_of_options {
        &POSITIONAL_ONLY_SUBCOMMAND
    } else {
        sub
    }
}

#[derive(Debug, Clone)]
pub struct ResolveResult<'a> {
    pub subcommand: &'a Subcommand,
    /// The subcommand `subcommand` was reached from, if any. The `help`
    /// template completes the parent's subcommands.
    pub parent_subcommand: Option<&'a Subcommand>,
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
    /// True once a bare `--` was seen: every remaining token is positional.
    /// `persistent_options` is empty and `subcommand` carries no subcommand
    /// list, so neither can be offered past that point.
    pub end_of_options: bool,
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
        end_of_options: false,
        case_insensitive: false,
        parent: None,
        positional_args: &root.args,
    };
    resolve_loop(Transition::Subcommand(rest, root, ctx, false, false))
}

/// Resolve with a registry reference — enables `LoadSpec::SpecPath` lookups
/// when a subcommand declares `load_spec` as a name reference (e.g.
/// `aws/ec2`). The target spec must already be in the registry.
pub fn resolve_with_registry<'a>(
    registry: &'a Registry,
    root: &'a Subcommand,
    tokens: &'a [CommandToken],
) -> ResolveResult<'a> {
    resolve_with_options(registry, root, tokens, false)
}

/// `case_insensitive` selects PowerShell/cmd name matching.
pub fn resolve_with_options<'a>(
    registry: &'a Registry,
    root: &'a Subcommand,
    tokens: &'a [CommandToken],
    case_insensitive: bool,
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
        end_of_options: false,
        case_insensitive,
        parent: None,
        positional_args: &root.args,
    };
    resolve_loop(Transition::Subcommand(rest, root, ctx, false, false))
}

struct Ctx<'a> {
    persistent: Vec<&'a Opt>,
    accepted_options: Vec<String>,
    positional_args_consumed: bool,
    registry: Option<&'a Registry>,
    /// Set once a bare `--` has been seen. Everything after it is positional,
    /// so options must no longer be offered.
    end_of_options: bool,
    /// PowerShell and cmd match command and option names case-insensitively,
    /// so a typed `-path` must resolve `-Path`. POSIX shells do not.
    case_insensitive: bool,
    /// The subcommand we descended *from*. The `help` template completes its
    /// siblings: `git help <TAB>` lists `git`'s subcommands, not `help`'s.
    parent: Option<&'a Subcommand>,
    /// Remaining command positionals. Option arguments use a separate slice,
    /// so returning from an option cannot restart the command's argument list.
    positional_args: &'a [Arg],
}

/// The tokenizer marks every word after `--` raw without knowing whether an
/// option consumed that `--` as its required value. Only the resolver knows
/// when option parsing actually ended (#141).
fn is_option_token(token: &CommandToken, ctx: &Ctx<'_>) -> bool {
    !ctx.end_of_options
        && (token.is_option || (token.is_raw && !token.is_quoted && token.token.starts_with('-')))
}

/// If `sub` has a `load_spec: SpecPath`, resolve it against the registry and
/// return the loaded spec; otherwise return `sub` unchanged.
fn maybe_substitute<'a>(sub: &'a Subcommand, ctx: &Ctx<'a>) -> &'a Subcommand {
    if let (Some(LoadSpec::SpecPath { name }), Some(reg)) = (&sub.load_spec, ctx.registry)
        && let Some(loaded) = reg.get(name)
    {
        return loaded;
    }
    sub
}

/// Explicit resolver continuations keep token traversal on a constant stack
/// (#128). A pasted argument list must not overflow the completion worker.
enum Transition<'a> {
    Subcommand(&'a [CommandToken], &'a Subcommand, Ctx<'a>, bool, bool),
    Option(&'a [CommandToken], &'a Opt, &'a Subcommand, Ctx<'a>),
    Arg(
        &'a [CommandToken],
        &'a [Arg],
        &'a Subcommand,
        Ctx<'a>,
        bool,
        bool,
    ),
    Finished(ResolveResult<'a>),
}

fn resolve_loop(mut next: Transition<'_>) -> ResolveResult<'_> {
    loop {
        next = match next {
            Transition::Subcommand(tokens, sub, ctx, args_depleted, args_used) => {
                step_subcommand(tokens, sub, ctx, args_depleted, args_used)
            }
            Transition::Option(tokens, opt, sub, ctx) => step_option(tokens, opt, sub, ctx),
            Transition::Arg(tokens, args, sub, ctx, from_option, from_variadic) => {
                step_arg(tokens, args, sub, ctx, from_option, from_variadic)
            }
            Transition::Finished(result) => return result,
        };
    }
}

fn step_subcommand<'a>(
    tokens: &'a [CommandToken],
    sub: &'a Subcommand,
    ctx: Ctx<'a>,
    args_depleted: bool,
    _args_used: bool,
) -> Transition<'a> {
    let args_depleted = args_depleted || (!sub.args.is_empty() && ctx.positional_args.is_empty());
    // Base case: no tokens left — suggest at this subcommand's level.
    if tokens.is_empty() {
        return Transition::Finished(ResolveResult {
            subcommand: surface(&ctx, sub),
            parent_subcommand: ctx.parent,
            active_arg: ctx.positional_args.first(),
            persistent_options: visible_options(&ctx, &sub.options),
            args_depleted,
            active_partial: None,
            from_option: false,
            accepted_option_tokens: ctx.accepted_options,
            positional_args_consumed: ctx.positional_args_consumed,
            end_of_options: ctx.end_of_options,
        });
    }

    // If the first token is incomplete, that's the partial the user is typing.
    if !tokens[0].complete {
        return Transition::Finished(ResolveResult {
            subcommand: surface(&ctx, sub),
            parent_subcommand: ctx.parent,
            active_arg: ctx.positional_args.first(),
            persistent_options: visible_options(&ctx, &sub.options),
            args_depleted,
            active_partial: Some(&tokens[0]),
            from_option: false,
            accepted_option_tokens: ctx.accepted_options,
            positional_args_consumed: ctx.positional_args_consumed,
            end_of_options: ctx.end_of_options,
        });
    }

    let active = &tokens[0];
    let all_opts: Vec<&Opt> = ctx
        .persistent
        .iter()
        .copied()
        .chain(sub.options.iter())
        .collect();

    // A bare, completed `--` ends option parsing. The tokenizer marks the
    // tokens *after* it as raw but leaves `--` itself flagged as an option, so
    // it fell into the unknown-option branch below and stopped resolution —
    // `git log -- <TAB>` offered nothing instead of completing file paths.
    if is_option_token(active, &ctx) && active.token == "--" && active.complete {
        let mut new_ctx = ctx;
        new_ctx.end_of_options = true;
        return Transition::Subcommand(&tokens[1..], sub, new_ctx, args_depleted, false);
    }

    // Option?
    if is_option_token(active, &ctx) {
        if let Some(opt) = find_option(&all_opts, &active.token, ctx.case_insensitive) {
            let mut new_ctx = ctx;
            new_ctx.accepted_options.push(active.token.clone());
            return Transition::Option(tokens, opt, sub, new_ctx);
        }
        // `--opt<sep>value` where `<sep>` is not `=`. The tokenizer only ever
        // splits on `=`, so such an option never matched at all and its value
        // was silently dropped.
        if let Some((opt, _value)) =
            find_separated_option(&all_opts, &active.token, ctx.case_insensitive)
        {
            let mut new_ctx = ctx;
            new_ctx.accepted_options.push(opt.names[0].clone());
            return Transition::Subcommand(&tokens[1..], sub, new_ctx, false, false);
        }

        // Clustered short flags: `-abc` means `-a -b -c`. Each letter is
        // recorded as accepted so `exclusiveOn`/`dependsOn` see them.
        if let Some((cluster, attached_value)) = expand_short_cluster(&all_opts, &active.token) {
            let mut new_ctx = ctx;
            for opt in &cluster {
                new_ctx.accepted_options.push(opt.names[0].clone());
            }
            // If the final flag takes an argument and none was attached in
            // this token (`-p2222` attaches it), the next token feeds it.
            if let Some(last) = cluster.last()
                && !last.args.is_empty()
                && !attached_value
            {
                return Transition::Arg(&tokens[1..], &last.args, sub, new_ctx, true, false);
            }
            return Transition::Subcommand(&tokens[1..], sub, new_ctx, false, false);
        }

        // Unknown option — skip it and keep resolving the rest of the line
        // (#94). Returning here with no partial used to discard a valid
        // subcommand and the token being typed, dumping the unfiltered
        // top-level list; the flag simply is not ours to complete.
        return Transition::Subcommand(&tokens[1..], sub, ctx, args_depleted, false);
    }

    // Past an actual separator, nothing may resolve as a subcommand. A raw
    // tokenizer hint alone is insufficient: `--` may have been option data.
    if !ctx.end_of_options {
        // Subcommand?
        if let Some(next) = find_subcommand(sub, &active.token, ctx.case_insensitive) {
            let mut new_ctx = ctx;
            for opt in &sub.options {
                if opt.is_persistent && !new_ctx.persistent.iter().any(|o| opts_eq(o, opt)) {
                    new_ctx.persistent.push(opt);
                }
            }
            // LoadSpec::SpecPath substitution: if the matched subcommand is
            // just a stub pointing to another spec, load it lazily.
            new_ctx.parent = Some(sub);
            let resolved = maybe_substitute(next, &new_ctx);
            new_ctx.positional_args = &resolved.args;
            return Transition::Subcommand(&tokens[1..], resolved, new_ctx, false, false);
        }
    }

    // Positional arg?
    if !ctx.positional_args.is_empty() {
        return Transition::Arg(tokens, ctx.positional_args, sub, ctx, false, false);
    }

    // Nothing matched — an unknown command word cannot be completed past
    // (#98). Skipping it used to restart completion at this level, so
    // `git chekout ` offered every git subcommand and accepting one
    // produced `git chekout archive`.
    Transition::Finished(ResolveResult {
        subcommand: &EMPTY_SUBCOMMAND,
        parent_subcommand: ctx.parent,
        active_arg: None,
        persistent_options: Vec::new(),
        args_depleted: true,
        active_partial: None,
        from_option: false,
        accepted_option_tokens: ctx.accepted_options,
        positional_args_consumed: ctx.positional_args_consumed,
        end_of_options: ctx.end_of_options,
    })
}

fn step_option<'a>(
    tokens: &'a [CommandToken],
    opt: &'a Opt,
    sub: &'a Subcommand,
    ctx: Ctx<'a>,
) -> Transition<'a> {
    // If the option takes args, the next token(s) feed them.
    if !opt.args.is_empty() {
        if opt.requires_separator.is_some() && !tokens[0].separated {
            // The value must be attached (`-i=val`). Reaching here with an
            // unattached option means none was given.
            if tokens.len() <= 1 {
                // `-i ` — upstream suppresses all suggestions.
                return Transition::Finished(ResolveResult {
                    subcommand: &EMPTY_SUBCOMMAND,
                    parent_subcommand: ctx.parent,
                    active_arg: None,
                    persistent_options: Vec::new(),
                    args_depleted: true,
                    active_partial: None,
                    from_option: true,
                    accepted_option_tokens: ctx.accepted_options,
                    positional_args_consumed: ctx.positional_args_consumed,
                    end_of_options: false,
                });
            }
            // `-i val` — `val` belongs to the command, not to this option.
            // Consuming it as the option's value invented an argument the
            // shell would never pass.
            return Transition::Subcommand(&tokens[1..], sub, ctx, false, false);
        }
        return Transition::Arg(&tokens[1..], &opt.args, sub, ctx, true, false);
    }
    // No args — consume the option token and continue at subcommand level.
    Transition::Subcommand(&tokens[1..], sub, ctx, false, false)
}

fn step_arg<'a>(
    tokens: &'a [CommandToken],
    args: &'a [Arg],
    sub: &'a Subcommand,
    ctx: Ctx<'a>,
    from_option: bool,
    from_variadic: bool,
) -> Transition<'a> {
    if args.is_empty() {
        return Transition::Subcommand(tokens, sub, ctx, !from_option, !from_option);
    }

    // No tokens left → suggest for the first remaining arg.
    if tokens.is_empty() {
        return Transition::Finished(ResolveResult {
            subcommand: surface(&ctx, sub),
            parent_subcommand: ctx.parent,
            active_arg: Some(&args[0]),
            persistent_options: visible_options(&ctx, &sub.options),
            args_depleted: false,
            active_partial: None,
            from_option,
            accepted_option_tokens: ctx.accepted_options,
            positional_args_consumed: ctx.positional_args_consumed,
            end_of_options: ctx.end_of_options,
        });
    }

    // Required option values are whole shell words, even when the parser
    // split a flag-looking value at `=` (#141). Source spans distinguish
    // attached fragments from the next whitespace-separated argument.
    let required_option_value = from_option && !args[0].is_optional && !from_variadic;
    let (active, consumed) = if required_option_value {
        tokens[0]
            .unsplit
            .as_ref()
            .map(|word| (word.token(), word.parts()))
            .unwrap_or((&tokens[0], 1))
    } else {
        (&tokens[0], 1)
    };
    // First word incomplete → active partial.
    if !active.complete {
        return Transition::Finished(ResolveResult {
            subcommand: surface(&ctx, sub),
            parent_subcommand: ctx.parent,
            active_arg: Some(&args[0]),
            persistent_options: visible_options(&ctx, &sub.options),
            args_depleted: false,
            active_partial: Some(active),
            from_option,
            accepted_option_tokens: ctx.accepted_options,
            positional_args_consumed: ctx.positional_args_consumed,
            end_of_options: ctx.end_of_options,
        });
    }

    // A required option value is data even when it spells a known flag or
    // `--` (e.g. `grep -e -- file`). Once a variadic arg has received its
    // required first value, retain the usual option-breaking behavior.
    // A separator does not consume a positional slot, including when we are
    // already partway through the command's argument list (#120).
    if !required_option_value && is_option_token(active, &ctx) && active.token == "--" {
        let mut new_ctx = ctx;
        new_ctx.end_of_options = true;
        return Transition::Arg(&tokens[1..], args, sub, new_ctx, from_option, false);
    }
    // An option token is an option wherever it appears — including after a
    // required positional (`grep foo -l`, #97). Checking only when every
    // remaining arg was optional swallowed such flags as positional values,
    // so they never reached `accepted_option_tokens` and exclusiveOn /
    // dependsOn / repeat limits silently stopped applying.
    let all_opts: Vec<&Opt> = ctx
        .persistent
        .iter()
        .copied()
        .chain(sub.options.iter())
        .collect();
    if !required_option_value && is_option_token(active, &ctx) {
        if let Some(opt) = find_option(&all_opts, &active.token, ctx.case_insensitive) {
            let mut new_ctx = ctx;
            new_ctx.accepted_options.push(active.token.clone());
            return Transition::Option(tokens, opt, sub, new_ctx);
        }
        if let Some((opt, _value)) =
            find_separated_option(&all_opts, &active.token, ctx.case_insensitive)
        {
            let mut new_ctx = ctx;
            new_ctx.accepted_options.push(opt.names[0].clone());
            return Transition::Subcommand(&tokens[1..], sub, new_ctx, false, false);
        }
        if let Some((cluster, attached_value)) = expand_short_cluster(&all_opts, &active.token) {
            let mut new_ctx = ctx;
            for opt in &cluster {
                new_ctx.accepted_options.push(opt.names[0].clone());
            }
            if let Some(last) = cluster.last()
                && !last.args.is_empty()
                && !attached_value
            {
                return Transition::Arg(&tokens[1..], &last.args, sub, new_ctx, true, false);
            }
            return Transition::Subcommand(&tokens[1..], sub, new_ctx, false, false);
        }
        if args.iter().all(|a| a.is_optional) {
            return Transition::Finished(ResolveResult {
                subcommand: surface(&ctx, sub),
                parent_subcommand: ctx.parent,
                active_arg: Some(&args[0]),
                persistent_options: visible_options(&ctx, &sub.options),
                args_depleted: false,
                active_partial: None,
                from_option,
                accepted_option_tokens: ctx.accepted_options,
                positional_args_consumed: ctx.positional_args_consumed,
                end_of_options: ctx.end_of_options,
            });
        }
        // Unknown dash-token with required args outstanding: fall through
        // and let it be consumed as a positional value (e.g. a pattern).
    }
    // If all remaining args are optional, the user may be skipping to a
    // subcommand instead.
    if args.iter().all(|a| a.is_optional)
        && !ctx.end_of_options
        && let Some(next) = find_subcommand(sub, &active.token, ctx.case_insensitive)
    {
        // Inherit this level's persistent options, exactly as
        // `run_subcommand` does. Omitting it meant `cmd --optWithOptionalArg
        // subcmd` dropped every parent `isPersistent` option.
        let mut new_ctx = ctx;
        for opt in &sub.options {
            if opt.is_persistent && !new_ctx.persistent.iter().any(|o| opts_eq(o, opt)) {
                new_ctx.persistent.push(opt);
            }
        }
        new_ctx.parent = Some(sub);
        let resolved = maybe_substitute(next, &new_ctx);
        new_ctx.positional_args = &resolved.args;
        return Transition::Subcommand(&tokens[1..], resolved, new_ctx, false, false);
    }

    let active_arg = &args[0];

    // `isCommand` arg: this token names another command (e.g. `sudo ls`,
    // `time ls`, `env ls`, `strace ls`). Load that command's spec from the
    // registry and resolve the remaining tokens against it, in a fresh
    // context. Mirrors the `load_spec` substitution path above.
    if active_arg.is_command
        && !ctx.end_of_options
        && let Some(reg) = ctx.registry
        && let Some(loaded) = reg.get(&active.token).or_else(|| {
            // PowerShell/cmd command names are case-insensitive (#106).
            if ctx.case_insensitive {
                reg.get(&active.token.to_lowercase())
            } else {
                None
            }
        })
    {
        let nested = Ctx {
            persistent: Vec::new(),
            accepted_options: Vec::new(),
            positional_args_consumed: false,
            registry: ctx.registry,
            end_of_options: false,
            case_insensitive: ctx.case_insensitive,
            parent: None,
            positional_args: &loaded.args,
        };
        return Transition::Subcommand(&tokens[consumed..], loaded, nested, false, false);
    }

    if active_arg.is_variadic {
        let mut new_ctx = ctx;
        if !from_option {
            new_ctx.positional_args_consumed = true;
        }
        return Transition::Arg(&tokens[consumed..], args, sub, new_ctx, from_option, true);
    }

    // Move to the next positional arg definition.
    let mut new_ctx = ctx;
    if !from_option {
        new_ctx.positional_args_consumed = true;
        new_ctx.positional_args = &args[1..];
    }
    Transition::Arg(
        &tokens[consumed..],
        &args[1..],
        sub,
        new_ctx,
        from_option,
        false,
    )
}

fn find_option<'a>(opts: &[&'a Opt], token: &str, case_insensitive: bool) -> Option<&'a Opt> {
    opts.iter().copied().find(|o| {
        if case_insensitive {
            o.names.iter().any(|n| {
                n.eq_ignore_ascii_case(token)
                    || n.strip_suffix('=')
                        .is_some_and(|bare| bare.eq_ignore_ascii_case(token))
            })
        } else {
            o.matches(token)
        }
    })
}

fn find_subcommand<'a>(
    sub: &'a Subcommand,
    token: &str,
    case_insensitive: bool,
) -> Option<&'a Subcommand> {
    sub.subcommands.iter().find(|s| {
        if case_insensitive {
            s.names.iter().any(|n| n.eq_ignore_ascii_case(token))
        } else {
            s.matches(token)
        }
    })
}

/// Match `--opt<sep>value` for an option declaring a non-`=` `requiresSeparator`
/// (e.g. `-D:name`). Returns the option and the attached value.
fn find_separated_option<'a>(
    opts: &[&'a Opt],
    token: &str,
    case_insensitive: bool,
) -> Option<(&'a Opt, String)> {
    for opt in opts.iter().copied() {
        let Some(sep) = opt.requires_separator.as_deref() else {
            continue;
        };
        if sep.is_empty() || sep == "=" {
            // `=` is already split by the tokenizer.
            continue;
        }
        for name in &opt.names {
            let prefix = format!("{name}{sep}");
            // `get(..n)` returns None when n is not a char boundary; slicing
            // directly panicked the whole process on any token whose bytes
            // straddle the prefix length with a multi-byte char — e.g. an
            // option value typed after a CJK positional (#54).
            let matched = if case_insensitive {
                token
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
            } else {
                token.starts_with(&prefix)
            };
            if matched && let Some(rest) = token.get(prefix.len()..) {
                return Some((opt, rest.to_string()));
            }
        }
    }
    None
}

/// Expand `-abc` into the options `-a`, `-b`, `-c` when every letter names a
/// known single-character flag. Returns the cluster plus whether the final
/// flag's value was attached in the same token (`-p2222`, `-mfoo`): per POSIX
/// an arg-taking short flag consumes the rest of the token as its value, so
/// `-p2222` is `-p 2222`, not a cluster of unknown letters (#96). Returns
/// `None` if any leading letter is unknown, so a genuinely unknown option
/// still reports as unknown.
fn expand_short_cluster<'a>(opts: &[&'a Opt], token: &str) -> Option<(Vec<&'a Opt>, bool)> {
    let letters = token.strip_prefix('-')?;
    if letters.is_empty() || letters.starts_with('-') || letters.chars().count() < 2 {
        return None;
    }
    let count = letters.chars().count();
    let mut cluster = Vec::with_capacity(count);
    for (idx, ch) in letters.chars().enumerate() {
        let name = format!("-{ch}");
        let opt = find_option(opts, &name, false)?;
        if !opt.args.is_empty() {
            cluster.push(opt);
            // Attached value present unless this flag is the last char.
            return Some((cluster, idx + 1 != count));
        }
        cluster.push(opt);
    }
    Some((cluster, false))
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

/// Options visible to the caller. After a bare `--` there are none: every
/// remaining token is a positional argument.
fn visible_options<'a>(ctx: &Ctx<'a>, local: &'a [Opt]) -> Vec<&'a Opt> {
    if ctx.end_of_options {
        return Vec::new();
    }
    merged_options(&ctx.persistent, local)
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

    /// `find_separated_option` used to slice the token at the prefix's BYTE
    /// length before comparing; a token whose multi-byte char straddles that
    /// offset (e.g. `éé:x` against prefix `-I:`) panicked the process (#54).
    #[test]
    fn separated_option_slice_is_char_boundary_safe() {
        let opt = Opt {
            names: vec!["-I".to_string()],
            requires_separator: Some(":".to_string()),
            ..Default::default()
        };
        let opts = vec![&opt];
        // Pre-fix this line panicked: byte offset 3 lands inside the second é.
        assert!(find_separated_option(&opts, "éé:x", true).is_none());
        // A real match still extracts its (multi-byte) value intact.
        let hit = find_separated_option(&opts, "-I:héllo", false);
        assert_eq!(
            hit.map(|(o, v)| (o.names[0].clone(), v)),
            Some(("-I".into(), "héllo".into()))
        );
    }

    /// #94: an unrecognized option must not abort the whole line.
    #[test]
    fn unknown_option_is_skipped_not_fatal() {
        let status = make_spec("status", vec![], vec![make_opt("--short")]);
        let root = make_spec("git", vec![status], vec![]);
        let tokens = tok("git --bogus status --sh");
        let result = resolve(&root, &tokens);
        assert_eq!(result.subcommand.names[0], "status");
        assert_eq!(result.active_partial.unwrap().token, "--sh");
    }

    /// #98: an unknown command word cannot be completed past.
    #[test]
    fn unknown_subcommand_word_yields_no_surface() {
        let root = make_spec("git", vec![make_spec("checkout", vec![], vec![])], vec![]);
        let tokens = tok("git chekout ");
        let result = resolve(&root, &tokens);
        assert!(result.subcommand.names.iter().all(|n| n.is_empty()));
        assert!(result.persistent_options.is_empty());
        assert!(result.active_arg.is_none());
    }

    /// #96: `-p2222` consumes 2222 as -p's attached value.
    #[test]
    fn attached_short_option_value_is_consumed() {
        let port = Opt {
            names: vec!["-p".to_string()],
            args: vec![make_arg()],
            ..Default::default()
        };
        let root = make_spec("ssh", vec![], vec![port, make_opt("-v")]);
        let tokens = tok("ssh -p2222 -v");
        let result = resolve(&root, &tokens);
        assert!(
            result.accepted_option_tokens.contains(&"-p".to_string()),
            "{:?}",
            result.accepted_option_tokens
        );
        assert_eq!(result.active_partial.unwrap().token, "-v");
    }

    /// #97: an option after a required positional is still an option.
    #[test]
    fn option_after_required_positional_is_recorded() {
        let grep = Subcommand {
            names: vec!["grep".to_string()],
            args: vec![make_arg()],
            options: vec![make_opt("-l"), make_opt("--files-without-match")],
            ..Default::default()
        };
        let tokens = tok("grep foo -l --files-without");
        let result = resolve(&grep, &tokens);
        assert!(
            result.accepted_option_tokens.contains(&"-l".to_string()),
            "{:?}",
            result.accepted_option_tokens
        );
    }

    fn make_arg() -> Arg {
        Arg::default()
    }

    /// #128: completion runs on a worker stack. Every token transition must
    /// remain iterative for both options and variadic positional arguments.
    #[test]
    fn long_command_lines_resolve_on_a_small_worker_stack() {
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(|| {
                let root = Subcommand {
                    names: vec!["cmd".into()],
                    args: vec![Arg {
                        name: Some("files".into()),
                        is_variadic: true,
                        ..Default::default()
                    }],
                    options: vec![
                        make_opt("-v"),
                        Opt {
                            args: vec![make_arg()],
                            ..make_opt("-m")
                        },
                    ],
                    ..Default::default()
                };
                for (words, accepted) in [
                    ("-v ", 20_000),
                    ("-m value ", 20_000),
                    ("--unknown ", 0),
                    ("file ", 0),
                ] {
                    let line = format!("cmd {}partial", words.repeat(20_000));
                    let tokens = tok(&line);
                    let result = resolve(&root, &tokens);
                    assert_eq!(result.accepted_option_tokens.len(), accepted);
                    assert_eq!(result.active_partial.unwrap().token, "partial");
                    assert_eq!(
                        result.active_arg.and_then(|a| a.name.as_deref()),
                        Some("files")
                    );
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// #120: options and `--` between positionals must keep the remaining
    /// command argument slice, rather than restarting it at the first arg.
    #[test]
    fn interspersed_options_preserve_positional_cursor() {
        let root = Subcommand {
            names: vec!["cmd".into()],
            args: ["first", "second", "third"]
                .into_iter()
                .map(|name| Arg {
                    name: Some(name.into()),
                    ..Default::default()
                })
                .collect(),
            options: vec![
                make_opt("--verbose"),
                Opt {
                    args: vec![make_arg()],
                    ..make_opt("--mode")
                },
                Opt {
                    args: vec![make_arg()],
                    ..make_opt("-m")
                },
                Opt {
                    args: vec![make_arg()],
                    requires_separator: Some(":".into()),
                    ..make_opt("-D")
                },
            ],
            ..Default::default()
        };
        for line in [
            "cmd one --verbose ",
            "cmd one --mode chosen ",
            "cmd one --mode=chosen ",
            "cmd one -mchosen ",
            "cmd one -D:chosen ",
            "cmd one -- ",
        ] {
            let tokens = tok(line);
            let result = resolve(&root, &tokens);
            assert_eq!(
                result.active_arg.and_then(|a| a.name.as_deref()),
                Some("second"),
                "{line}"
            );
            assert!(!result.from_option, "{line}");
            assert!(!result.args_depleted, "{line}");
        }
        let tokens = tok("cmd one -- two ");
        let result = resolve(&root, &tokens);
        assert!(result.end_of_options);
        assert!(result.persistent_options.is_empty());
        assert_eq!(
            result.active_arg.and_then(|a| a.name.as_deref()),
            Some("third")
        );

        // An exhausted argument list stays exhausted through later options.
        for line in ["cmd one two three ", "cmd one two three --verbose "] {
            let tokens = tok(line);
            let result = resolve(&root, &tokens);
            assert!(result.active_arg.is_none(), "{line}");
            assert!(result.args_depleted, "{line}");
        }
    }

    #[test]
    fn variadic_positionals_keep_their_cursor_across_options_and_separator() {
        let root = Subcommand {
            names: vec!["cmd".into()],
            args: vec![
                make_arg(),
                Arg {
                    name: Some("rest".into()),
                    is_variadic: true,
                    ..Default::default()
                },
            ],
            options: vec![make_opt("--verbose")],
            ..Default::default()
        };
        for line in [
            "cmd one two --verbose ",
            "cmd one two -- ",
            "cmd one two -- --verbose ",
        ] {
            let tokens = tok(line);
            let result = resolve(&root, &tokens);
            assert_eq!(
                result.active_arg.and_then(|a| a.name.as_deref()),
                Some("rest"),
                "{line}"
            );
            if line.contains(" -- ") {
                assert!(result.end_of_options);
                assert!(result.persistent_options.is_empty());
            }
        }
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
        // Past the bare `--` the surface is the positional-only stand-in;
        // `end_of_options` records that resolution stayed at `log`'s level,
        // and the raw token is completed as a plain positional.
        assert!(result.end_of_options);
        let partial = result.active_partial.expect("raw token is being typed");
        assert_eq!(partial.token, "file1");
        assert!(partial.is_raw);
    }

    #[test]
    fn resolve_unknown_subcommand_stays_at_root() {
        // #98 changed this contract: an unknown command word yields no
        // suggestion surface at all (previously resolution stayed at root
        // and every subcommand was offered past the typo).
        let root = make_spec("git", vec![make_spec("status", vec![], vec![])], vec![]);
        let tokens = tok("git nonexistent ");
        let result = resolve(&root, &tokens);
        assert!(result.subcommand.names.iter().all(|n| n.is_empty()));
        assert!(result.persistent_options.is_empty());
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

    /// A bare `--` ends option parsing: the resolver must continue into
    /// positional args and stop offering options.
    #[test]
    fn bare_double_dash_enables_positional_completion() {
        let mut spec = make_spec("git", vec![], vec![make_opt("--follow")]);
        spec.args = vec![Arg {
            name: Some("path".into()),
            is_optional: true,
            ..Default::default()
        }];
        let tokens = crate::spec::parse_command("git -- ");
        let result = resolve(&spec, &tokens);
        assert!(
            result.active_arg.is_some(),
            "positional arg must be active after `--`"
        );
        assert!(
            result.persistent_options.is_empty(),
            "options must not be offered after `--`"
        );

        // Before `--`, options are still in scope.
        let tokens = crate::spec::parse_command("git ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.persistent_options.len(), 1);
    }

    /// #141: option values must win over both real flags and the parser's
    /// context-free end-of-options annotations.
    #[test]
    fn required_option_values_are_data_before_option_dispatch() {
        let mut pattern = make_opt("-e");
        pattern.args = vec![Arg {
            name: Some("pattern".into()),
            ..Default::default()
        }];
        let mut spec = make_spec("grep", vec![], vec![pattern, make_opt("-n")]);
        spec.args = vec![Arg {
            name: Some("file".into()),
            ..Default::default()
        }];

        for line in ["grep -e -- ", "grep -e -n ", "grep -e=-- "] {
            let tokens = tok(line);
            let result = resolve(&spec, &tokens);
            assert_eq!(
                result.active_arg.and_then(|arg| arg.name.as_deref()),
                Some("file"),
                "{line}"
            );
            assert!(!result.from_option, "{line}");
            assert!(!result.end_of_options, "{line}");
            assert_eq!(result.accepted_option_tokens, ["-e"], "{line}");
        }

        // A literal `--` must not disable flags later in the command.
        let tokens = tok("grep -e -- -n ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.accepted_option_tokens, ["-e", "-n"]);
        assert!(!result.end_of_options);
        assert_eq!(
            result.active_arg.and_then(|a| a.name.as_deref()),
            Some("file")
        );

        // A subsequent actual separator still ends option parsing.
        let tokens = tok("grep -e -- -- -n ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.accepted_option_tokens, ["-e"]);
        assert!(result.end_of_options);
        assert!(result.args_depleted);
    }

    #[test]
    fn required_option_values_consume_attached_equals_as_one_word() {
        let mut pattern = make_opt("-e");
        pattern.args = vec![Arg {
            name: Some("pattern".into()),
            ..Default::default()
        }];
        let mut define = pattern.clone();
        define.names = vec!["--define".into()];
        define.requires_separator = Some("=".into());
        let mut spec = make_spec("audit", vec![], vec![pattern, define]);
        spec.args = ["first", "second"]
            .map(|name| Arg {
                name: Some(name.into()),
                ..Default::default()
            })
            .to_vec();

        for line in [
            "audit -e --flag=value ",
            "audit -e '--flag=value' ",
            "audit -e --flag=\"va lue\" ",
            "audit -e --outer=--flag=value ",
            "audit -e=--flag=value ",
            "audit --define=--flag=value ",
        ] {
            let tokens = tok(line);
            let result = resolve(&spec, &tokens);
            assert_eq!(
                result.active_arg.and_then(|a| a.name.as_deref()),
                Some("first"),
                "{line}"
            );
            assert!(!result.from_option, "{line}");
        }
        for line in [
            "audit -e --flag=value file ",
            "audit -e '--flag=value' file ",
            "audit -e --flag= value ",
            "audit --define=--flag=value file ",
            // requiresSeparator must still leave whitespace-separated values alone.
            "audit --define file ",
        ] {
            let tokens = tok(line);
            let result = resolve(&spec, &tokens);
            assert_eq!(
                result.active_arg.and_then(|a| a.name.as_deref()),
                Some("second"),
                "{line}"
            );
            assert!(!result.from_option, "{line}");
        }
        for (line, partial, length) in [
            ("audit -e --flag=va", "--flag=va", 9),
            ("audit -e --flag=", "--flag=", 7),
            ("audit -e '--flag=va'", "--flag=va", 11),
            ("audit -e --flag=\"va l", "--flag=va l", 12),
            ("audit -e --outer=--flag=va", "--outer=--flag=va", 17),
            ("audit -e=--flag=va", "--flag=va", 9),
            ("audit --define=--flag=va", "--flag=va", 9),
            // An ordinary attached value still excludes its own option name.
            ("audit -e=va", "va", 2),
        ] {
            let tokens = tok(line);
            let result = resolve(&spec, &tokens);
            assert_eq!(
                result.active_arg.and_then(|a| a.name.as_deref()),
                Some("pattern"),
                "{line}"
            );
            assert!(result.from_option, "{line}");
            let active = result.active_partial.unwrap();
            assert_eq!(active.token, partial, "{line}");
            assert_eq!(active.token_length, length, "{line}");
        }
        for line in [
            "audit -e --flag=value file",
            "audit -e '--flag=value' file",
            "audit -e --flag=\"va lue\" file",
        ] {
            let tokens = tok(line);
            let result = resolve(&spec, &tokens);
            assert_eq!(
                result.active_arg.and_then(|a| a.name.as_deref()),
                Some("first"),
                "{line}"
            );
            assert_eq!(result.active_partial.unwrap().token, "file", "{line}");
            assert!(!result.from_option, "{line}");
        }
    }

    #[test]
    fn optional_option_values_still_allow_flags_and_subcommands() {
        let mut maybe = make_opt("--maybe");
        maybe.args = vec![Arg {
            name: Some("value".into()),
            is_optional: true,
            ..Default::default()
        }];
        let mut spec = make_spec(
            "cmd",
            vec![make_spec("child", vec![], vec![])],
            vec![maybe, make_opt("--flag")],
        );
        spec.args = vec![Arg {
            name: Some("file".into()),
            ..Default::default()
        }];
        let tokens = tok("cmd --maybe --flag ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.accepted_option_tokens, ["--maybe", "--flag"]);
        assert!(!result.from_option);
        assert_eq!(
            result.active_arg.and_then(|a| a.name.as_deref()),
            Some("file")
        );
        let tokens = tok("cmd --maybe child ");
        assert_eq!(resolve(&spec, &tokens).subcommand.name(), "child");
    }

    /// `-abc` means `-a -b -c`. Each letter is recorded as accepted.
    #[test]
    fn clustered_short_flags_expand() {
        let spec = make_spec(
            "cmd",
            vec![],
            vec![make_opt("-a"), make_opt("-b"), make_opt("-c")],
        );
        let tokens = crate::spec::parse_command("cmd -abc ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.accepted_option_tokens, vec!["-a", "-b", "-c"]);
    }

    /// An unknown cluster stays unknown rather than being silently expanded.
    #[test]
    fn unknown_cluster_is_not_expanded() {
        let spec = make_spec("cmd", vec![], vec![make_opt("-a")]);
        let tokens = crate::spec::parse_command("cmd -az ");
        let result = resolve(&spec, &tokens);
        assert!(result.accepted_option_tokens.is_empty());
    }

    /// A `requiresSeparator` option only takes an *attached* value. Consuming
    /// the following whitespace-separated token invented an argument the shell
    /// would never have passed.
    #[test]
    fn requires_separator_does_not_consume_the_next_token() {
        let mut opt = make_opt("--define");
        opt.requires_separator = Some("=".into());
        opt.args = vec![Arg {
            name: Some("value".into()),
            ..Default::default()
        }];
        let sub = make_spec("child", vec![], vec![]);
        let spec = make_spec("cmd", vec![sub], vec![opt]);

        // `--define value` — `value` is not the option's value.
        let tokens = crate::spec::parse_command("cmd --define child ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.subcommand.name(), "child");

        // `--define=` — the tokenizer attaches the value, so the option's own
        // arg is what the user is completing.
        let tokens = crate::spec::parse_command("cmd --define=");
        let result = resolve(&spec, &tokens);
        assert!(result.from_option);
        assert_eq!(
            result.active_arg.and_then(|a| a.name.as_deref()),
            Some("value")
        );
    }

    /// `-D:name` — a non-`=` separator. The option never matched at all
    /// before, so its value was silently dropped.
    #[test]
    fn non_equals_separator_matches_the_option() {
        let mut opt = make_opt("-D");
        opt.requires_separator = Some(":".into());
        opt.args = vec![Arg {
            name: Some("value".into()),
            ..Default::default()
        }];
        let spec = make_spec("cmd", vec![], vec![opt]);
        let tokens = crate::spec::parse_command("cmd -D:name ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.accepted_option_tokens, vec!["-D"]);
    }

    /// An option whose args are all optional must still pass the parent's
    /// persistent options down to a subcommand.
    #[test]
    fn optional_option_arg_preserves_persistent_options() {
        let mut persistent = make_opt("--verbose");
        persistent.is_persistent = true;
        let mut opt = make_opt("--maybe");
        opt.args = vec![Arg {
            name: Some("v".into()),
            is_optional: true,
            ..Default::default()
        }];
        let child = make_spec("child", vec![], vec![]);
        let spec = make_spec("cmd", vec![child], vec![persistent, opt]);

        let tokens = crate::spec::parse_command("cmd --maybe child ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.subcommand.name(), "child");
        assert!(
            result
                .persistent_options
                .iter()
                .any(|o| o.names[0] == "--verbose"),
            "parent persistent option was dropped: {:?}",
            result
                .persistent_options
                .iter()
                .map(|o| &o.names[0])
                .collect::<Vec<_>>()
        );
    }

    /// Descending records the parent so the `help` template can complete its
    /// siblings.
    #[test]
    fn parent_subcommand_is_recorded() {
        let child = make_spec("help", vec![], vec![]);
        let sibling = make_spec("commit", vec![], vec![]);
        let spec = make_spec("git", vec![child, sibling], vec![]);
        let tokens = crate::spec::parse_command("git help ");
        let result = resolve(&spec, &tokens);
        assert_eq!(result.subcommand.name(), "help");
        assert_eq!(result.parent_subcommand.map(|p| p.name()), Some("git"));
    }

    /// After a bare `--` everything is positional-only: options were already
    /// suppressed, but subcommand names were still offered. The suggestion
    /// surface must expose neither, while positional args keep resolving.
    #[test]
    fn double_dash_suppresses_subcommands_too() {
        let mut log = make_spec(
            "log",
            vec![make_spec("stat", vec![], vec![])],
            vec![make_opt("--follow")],
        );
        log.args = vec![Arg {
            name: Some("path".into()),
            is_variadic: true,
            ..Default::default()
        }];
        let root = make_spec("git", vec![log], vec![]);

        // Trailing space after `--`: no subcommand names, no options, and the
        // positional arg still drives suggestions.
        let tokens = tok("git log -- ");
        let result = resolve(&root, &tokens);
        assert!(result.end_of_options);
        assert!(
            result.subcommand.subcommands.is_empty(),
            "subcommand names must not be offered past `--`"
        );
        assert!(result.persistent_options.is_empty());
        assert_eq!(
            result.active_arg.and_then(|a| a.name.as_deref()),
            Some("path")
        );

        // A completed token that names a subcommand stays a plain positional:
        // it must not descend into `stat`.
        let tokens = tok("git log -- stat ");
        let result = resolve(&root, &tokens);
        assert!(result.end_of_options);
        assert!(result.active_arg.is_some());

        // Same for a partial token.
        let tokens = tok("git log -- st");
        let result = resolve(&root, &tokens);
        assert!(result.end_of_options);
        assert_eq!(result.active_partial.unwrap().token, "st");
        assert!(result.persistent_options.is_empty());

        // Without `--`, the same context offers both surfaces.
        let tokens = tok("git log ");
        let result = resolve(&root, &tokens);
        assert!(!result.end_of_options);
        assert_eq!(result.subcommand.subcommands.len(), 1);
        assert_eq!(result.persistent_options.len(), 1);
    }

    /// A bare `--` inside one level keeps suppressing at deeper levels too —
    /// there is no way back into option/subcommand space.
    #[test]
    fn double_dash_state_survives_descending() {
        let inner = make_spec("add", vec![], vec![make_opt("--force")]);
        let remote = make_spec("remote", vec![inner], vec![]);
        let root = make_spec("git", vec![remote], vec![]);
        let tokens = tok("git -- remote add ");
        let result = resolve(&root, &tokens);
        assert!(result.end_of_options);
        // "remote" was treated positionally, not descended into.
        assert!(result.subcommand.subcommands.is_empty());
        assert!(result.persistent_options.is_empty());
    }

    /// End to end through the suggestion engine (the consumer of this
    /// resolver): before a bare `--` subcommand names and options are
    /// offered; after it only the positional candidates survive.
    #[test]
    fn engine_offers_only_positional_suggestions_after_double_dash() {
        use crate::suggest::Engine;

        let mut log = make_spec(
            "log",
            vec![make_spec("stat", vec![], vec![])],
            vec![make_opt("--follow")],
        );
        log.args = vec![Arg {
            name: Some("path".into()),
            is_variadic: true,
            suggestions: vec![crate::spec::model::Suggestion {
                name: "tracked-file".into(),
                ..Default::default()
            }],
            ..Default::default()
        }];
        let mut registry = Registry::new_with_options(false);
        registry.insert(make_spec("git", vec![log], vec![]));
        let engine = Engine::new(registry, Vec::new());

        let kinds = |blob: &[crate::spec::model::Suggestion]| -> Vec<SuggestionType> {
            blob.iter().map(|s| s.suggestion_type).collect()
        };

        // Without `--`: subcommands and options are on the table.
        let blob = engine.suggest_blob("git log ", "/");
        assert!(kinds(&blob).contains(&SuggestionType::Subcommand));
        assert!(kinds(&blob).contains(&SuggestionType::Option));

        // After it: neither, but the positional suggestion still flows.
        let blob = engine.suggest_blob("git log -- ", "/");
        let after = kinds(&blob);
        assert!(
            !after.contains(&SuggestionType::Subcommand),
            "subcommand names leaked past `--`: {blob:?}"
        );
        assert!(
            !after.contains(&SuggestionType::Option),
            "options leaked past `--`: {blob:?}"
        );
        assert!(blob.iter().any(|s| s.name == "tracked-file"));
    }

    /// #95: while an option's argument is pending, the engine must not
    /// offer subcommands or more options for that slot.
    #[test]
    fn engine_offers_nothing_for_a_free_text_option_argument() {
        use crate::suggest::Engine;

        let msg = Opt {
            names: vec!["-m".to_string(), "--message".to_string()],
            args: vec![make_arg()],
            ..Default::default()
        };
        let commit = make_spec(
            "commit",
            vec![make_spec("sub", vec![], vec![])],
            vec![msg, make_opt("--all")],
        );
        let mut registry = Registry::new_with_options(false);
        registry.insert(make_spec("git", vec![commit], vec![]));
        let engine = Engine::new(registry, Vec::new());

        assert!(
            engine.suggest_blob("git commit -m ", "/").is_empty(),
            "option-argument slot offered command surface"
        );
        assert!(
            engine.suggest_blob("git commit --message ", "/").is_empty(),
            "option-argument slot offered command surface"
        );
        // Control: without the pending option, options are offered.
        assert!(
            !engine.suggest_blob("git commit ", "/").is_empty(),
            "plain commit context lost its suggestions"
        );
    }
}
