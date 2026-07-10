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
    };
    run_subcommand(rest, root, ctx, false, false)
}

#[derive(Clone)]
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
            parent_subcommand: ctx.parent,
            active_arg: sub.args.first(),
            persistent_options: visible_options(&ctx, &sub.options),
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
            parent_subcommand: ctx.parent,
            active_arg: sub.args.first(),
            persistent_options: visible_options(&ctx, &sub.options),
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

    // A bare, completed `--` ends option parsing. The tokenizer marks the
    // tokens *after* it as raw but leaves `--` itself flagged as an option, so
    // it fell into the unknown-option branch below and stopped resolution —
    // `git log -- <TAB>` offered nothing instead of completing file paths.
    if active.is_option && !active.is_raw && active.token == "--" && active.complete {
        let mut new_ctx = ctx.clone();
        new_ctx.end_of_options = true;
        return run_subcommand(&tokens[1..], sub, new_ctx, args_depleted, false);
    }

    // Option?
    if active.is_option && !active.is_raw {
        if let Some(opt) = find_option(&all_opts, &active.token, ctx.case_insensitive) {
            let mut new_ctx = ctx.clone();
            new_ctx.accepted_options.push(active.token.clone());
            return run_option(tokens, opt, sub, new_ctx);
        }
        // `--opt<sep>value` where `<sep>` is not `=`. The tokenizer only ever
        // splits on `=`, so such an option never matched at all and its value
        // was silently dropped.
        if let Some((opt, _value)) =
            find_separated_option(&all_opts, &active.token, ctx.case_insensitive)
        {
            let mut new_ctx = ctx.clone();
            new_ctx.accepted_options.push(opt.names[0].clone());
            return run_subcommand(&tokens[1..], sub, new_ctx, false, false);
        }

        // Clustered short flags: `-abc` means `-a -b -c`. Each letter is
        // recorded as accepted so `exclusiveOn`/`dependsOn` see them.
        if let Some(cluster) = expand_short_cluster(&all_opts, &active.token) {
            let mut new_ctx = ctx.clone();
            for opt in &cluster {
                new_ctx.accepted_options.push(opt.names[0].clone());
            }
            // If the final flag takes an argument, the next token feeds it.
            if let Some(last) = cluster.last() {
                if !last.args.is_empty() {
                    return run_arg(&tokens[1..], &last.args, sub, new_ctx, true, false);
                }
            }
            return run_subcommand(&tokens[1..], sub, new_ctx, false, false);
        }

        // Unknown option — fall through but don't match as subcommand.
        return ResolveResult {
            subcommand: sub,
            parent_subcommand: ctx.parent,
            active_arg: None,
            persistent_options: visible_options(&ctx, &sub.options),
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
        if let Some(next) = find_subcommand(sub, &active.token, ctx.case_insensitive) {
            let mut new_ctx = ctx.clone();
            for opt in &sub.options {
                if opt.is_persistent && !new_ctx.persistent.iter().any(|o| opts_eq(o, opt)) {
                    new_ctx.persistent.push(opt);
                }
            }
            // LoadSpec::SpecPath substitution: if the matched subcommand is
            // just a stub pointing to another spec, load it lazily.
            new_ctx.parent = Some(sub);
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
        if opt.requires_separator.is_some() && !tokens[0].separated {
            // The value must be attached (`-i=val`). Reaching here with an
            // unattached option means none was given.
            if tokens.len() <= 1 {
                // `-i ` — upstream suppresses all suggestions.
                return ResolveResult {
                    subcommand: &EMPTY_SUBCOMMAND,
                    parent_subcommand: ctx.parent,
                    active_arg: None,
                    persistent_options: Vec::new(),
                    args_depleted: true,
                    active_partial: None,
                    from_option: true,
                    accepted_option_tokens: ctx.accepted_options.clone(),
                    positional_args_consumed: ctx.positional_args_consumed,
                };
            }
            // `-i val` — `val` belongs to the command, not to this option.
            // Consuming it as the option's value invented an argument the
            // shell would never pass.
            return run_subcommand(&tokens[1..], sub, ctx, false, false);
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
            parent_subcommand: ctx.parent,
            active_arg: Some(&args[0]),
            persistent_options: visible_options(&ctx, &sub.options),
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
            parent_subcommand: ctx.parent,
            active_arg: Some(&args[0]),
            persistent_options: visible_options(&ctx, &sub.options),
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
            if let Some(opt) = find_option(&all_opts, &active.token, ctx.case_insensitive) {
                let mut new_ctx = ctx.clone();
                new_ctx.accepted_options.push(active.token.clone());
                return run_option(tokens, opt, sub, new_ctx);
            }
            return ResolveResult {
                subcommand: sub,
                parent_subcommand: ctx.parent,
                active_arg: Some(&args[0]),
                persistent_options: visible_options(&ctx, &sub.options),
                args_depleted: false,
                active_partial: None,
                from_option,
                accepted_option_tokens: ctx.accepted_options.clone(),
                positional_args_consumed: ctx.positional_args_consumed,
            };
        }
        if !active.is_raw {
            if let Some(next) = find_subcommand(sub, &active.token, ctx.case_insensitive) {
                // Inherit this level's persistent options, exactly as
                // `run_subcommand` does. Omitting it meant `cmd --optWithOptionalArg
                // subcmd` dropped every parent `isPersistent` option.
                let mut new_ctx = ctx.clone();
                for opt in &sub.options {
                    if opt.is_persistent && !new_ctx.persistent.iter().any(|o| opts_eq(o, opt)) {
                        new_ctx.persistent.push(opt);
                    }
                }
                new_ctx.parent = Some(sub);
                let resolved = maybe_substitute(next, &new_ctx);
                return run_subcommand(&tokens[1..], resolved, new_ctx, false, false);
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
                    end_of_options: false,
                    case_insensitive: ctx.case_insensitive,
                    parent: None,
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

fn find_option<'a>(opts: &[&'a Opt], token: &str, case_insensitive: bool) -> Option<&'a Opt> {
    opts.iter().copied().find(|o| {
        if case_insensitive {
            o.names.iter().any(|n| n.eq_ignore_ascii_case(token))
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
            let matched = if case_insensitive {
                token.len() >= prefix.len() && token[..prefix.len()].eq_ignore_ascii_case(&prefix)
            } else {
                token.starts_with(&prefix)
            };
            if matched {
                return Some((opt, token[prefix.len()..].to_string()));
            }
        }
    }
    None
}

/// Expand `-abc` into the options `-a`, `-b`, `-c` when every letter names a
/// known single-character flag. Returns `None` if any letter is unknown, so a
/// genuinely unknown option still reports as unknown.
fn expand_short_cluster<'a>(opts: &[&'a Opt], token: &str) -> Option<Vec<&'a Opt>> {
    let letters = token.strip_prefix('-')?;
    if letters.is_empty() || letters.starts_with('-') || letters.chars().count() < 2 {
        return None;
    }
    let mut cluster = Vec::with_capacity(letters.chars().count());
    for (idx, ch) in letters.chars().enumerate() {
        let name = format!("-{ch}");
        let opt = find_option(opts, &name, false)?;
        // Only the final flag in a cluster may take an argument.
        if !opt.args.is_empty() && idx + 1 != letters.chars().count() {
            return None;
        }
        cluster.push(opt);
    }
    Some(cluster)
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
}
