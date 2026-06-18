//! Hand-ported fallback specs — the documented escape hatch for closing
//! extractor gaps without round-tripping through the extractor pipeline.
//!
//! These OVERRIDE the bundled spec of the same name (see
//! `Registry::new_with_defaults`). Two specs live here today, both fixing
//! confirmed parity gaps:
//!   * `terraform` — upstream defines its subcommands STATICALLY as
//!     `subcommands: [...mainCommands, ...otherCommands, ...extraCommands]`
//!     (spreads of const arrays, NOT `generateSpec`). The extractor inlines
//!     spreads when they resolve to a literal array, but some element inside
//!     terraform's arrays evaluated to a non-array and tripped the
//!     `flipImpure` fallback (extract.ts:443-456), so the whole `subcommands`
//!     field was dropped — leaving only `-help`/`-chdir`/`-version`. The
//!     correct fix is in the extractor, but the `@withfig` source isn't
//!     present in this tree (so the bundle can't be regenerated); the 21
//!     subcommands are re-stated statically here as the pragmatic remedy.
//!   * `kill` — needs a process-id generator (upstream runs `ps`); the
//!     bundle's kill.json had only `-s`/`-l` because the pid generator was
//!     an opaque JS closure the extractor can't evaluate.

use crate::spec::model::{
    Arg, Generator, Opt, PostProcess, PostProcessKind, ScriptInput, Subcommand, Suggestion,
    SuggestionType, Template,
};

pub fn all() -> Vec<Subcommand> {
    vec![terraform(), kill()]
}

/// Augment a freshly-loaded bundled spec in place. Unlike `all()` (which
/// fully replaces a spec), these are surgical additions that close parity
/// gaps the static extractor left — missing options the extractor dropped,
/// opaque-JS generators it couldn't evaluate, and a field it lost. Applied
/// in `Registry::get` after deserialization, so they survive a future
/// re-extraction of the bundle (they live here, not in the bundle).
pub fn patch(spec: &mut Subcommand, name: &str) {
    match name {
        "yarn" => patch_yarn(spec),
        "gh" => patch_gh(spec),
        "make" => {
            patch_make(spec);
            patch_make_options(spec);
        }
        "asciinema" => patch_asciinema(spec),
        "cd" => patch_cd(spec),
        "git" => patch_git(spec),
        "systemctl" => patch_systemctl(spec),
        "tar" => patch_tar(spec),
        "npm" => patch_npm(spec),
        "tmux" => patch_tmux(spec),
        "pnpm" => patch_pnpm(spec),
        "composer" => patch_composer(spec),
        "php" => patch_php(spec),
        // python/node complete a script path (filepaths generator) and upstream
        // includes `..`; add it as a static entry (filepaths generators don't
        // synthesize `..` generally — that would wrongly add it to e.g.
        // `git checkout <file>`).
        "python" | "python3" | "node" => add_parent_suggestion(spec),
        _ => {}
    }
}

fn patch_composer(spec: &mut Subcommand) {
    add_option(spec, "--help", "Display help for a command");
    add_option(spec, "--version", "Display this application version");
}

fn patch_php(spec: &mut Subcommand) {
    add_option(spec, "--version", "Show PHP version");
    add_option(spec, "--help", "Show command line help");
}

fn patch_pnpm(spec: &mut Subcommand) {
    for s in &mut spec.subcommands {
        if s.names.iter().any(|n| n == "init") {
            s.priority = Some(60);
        }
    }
}

fn patch_tmux(spec: &mut Subcommand) {
    add_subcommand_alias(
        spec,
        "new-session",
        "Create a new session",
        &["new", "new-session"],
    );
    add_subcommand_alias(
        spec,
        "new-window",
        "Create a new window",
        &["neww", "new-window"],
    );
}

fn patch_make_options(spec: &mut Subcommand) {
    const ADD: &[(&str, &str)] = &[
        ("--always-make", "Unconditionally make all targets"),
        ("--assume-new", "Consider file to be infinitely new"),
        ("--assume-old", "Consider file to be very old and do not remake it"),
        ("--check-symlink-times", "Use the latest mtime between symlinks and target"),
        ("--directory", "Change to DIRECTORY before doing anything"),
        ("--dry-run", "Print commands without executing them"),
        ("--environment-overrides", "Environment variables override makefiles"),
        ("--eval", "Evaluate string as makefile syntax"),
        ("--ignore-errors", "Ignore errors from commands"),
        ("--include-dir", "Search directory for included makefiles"),
        ("--jobs", "Allow N jobs at once"),
        ("--jobserver-style", "Select the jobserver style"),
        ("--just-print", "Print commands without executing them"),
        ("--keep-going", "Keep going when some targets cannot be made"),
        ("--load-average", "Avoid starting jobs above a load average"),
        ("--makefile", "Read FILE as a makefile"),
        ("--max-load", "Avoid starting jobs above a load average"),
        ("--new-file", "Consider file to be infinitely new"),
        ("--no-keep-going", "Turn off keep-going mode"),
        ("--old-file", "Consider file to be very old and do not remake it"),
        ("--output-sync", "Synchronize output of parallel jobs"),
        ("--quiet", "Run no commands; exit status says if up to date"),
        ("--recon", "Print commands without executing them"),
        ("--shuffle", "Randomize goal and prerequisite ordering"),
        ("--silent", "Do not echo recipes"),
        ("--trace", "Print tracing information"),
        ("--variables", "Print make's internal database"),
        ("--what-if", "Consider file to be infinitely new"),
    ];
    for (name, description) in ADD {
        add_option(spec, name, description);
    }
}

fn add_subcommand_alias(
    spec: &mut Subcommand,
    name: &str,
    description: &str,
    existing_aliases: &[&str],
) {
    if spec
        .subcommands
        .iter()
        .any(|s| s.names.first().map(|n| n == name).unwrap_or(false))
    {
        return;
    }
    if !spec.subcommands.iter().any(|s| {
        existing_aliases
            .iter()
            .all(|alias| s.names.iter().any(|n| n == alias))
    }) {
        return;
    }
    spec.subcommands.push(sub(name, description));
}

fn add_option(spec: &mut Subcommand, name: &str, description: &str) {
    if spec.options.iter().any(|o| o.names.iter().any(|n| n == name)) {
        return;
    }
    spec.options.push(opt(name, description));
}

/// npm: the `run`/`run-script` subcommand is missing the `--workspace(s)`
/// options upstream lists.
fn patch_npm(spec: &mut Subcommand) {
    const ADD: &[(&str, &str, bool)] = &[
        ("--workspace", "Run the command in the context of the given workspace", true),
        ("--workspaces", "Run the command in the context of all workspaces", false),
    ];
    for s in &mut spec.subcommands {
        let is_run = s.names.iter().any(|n| n == "run" || n == "run-script");
        if !is_run {
            continue;
        }
        for (n, d, takes_arg) in ADD {
            if s.options.iter().any(|o| o.names.iter().any(|x| x == n)) {
                continue;
            }
            let mut o = opt(n, d);
            if *takes_arg {
                o.args = vec![Arg::default()];
            }
            s.options.push(o);
        }
    }
}

/// Append `..` (priority 50, so it sorts after listed paths) to a spec's
/// first argument — for commands whose path arg includes the parent dir.
fn add_parent_suggestion(spec: &mut Subcommand) {
    if let Some(arg) = spec.args.first_mut() {
        if !arg.suggestions.iter().any(|s| s.name == "..") {
            arg.suggestions.push(Suggestion {
                name: "..".to_string(),
                suggestion_type: SuggestionType::Folder,
                priority: Some(50),
                ..Default::default()
            });
        }
    }
}

/// tar: our snapshot dropped the core mode flags (`-c`/`-x`/`-t`/…) and a few
/// help/version options upstream lists. Re-add the missing ones.
fn patch_tar(spec: &mut Subcommand) {
    const ADD: &[(&str, &str)] = &[
        ("-A", "Append archive to the end of another archive"),
        ("-c", "Create a new archive"),
        ("-d", "Find differences between archive and file system"),
        ("-t", "List the contents of an archive"),
        ("-r", "Append files to the end of an archive"),
        ("-u", "Append files which are newer than the corresponding copy in the archive"),
        ("-x", "Extract files from an archive"),
        ("--delete", "Delete from the archive"),
        ("--test-label", "Test the archive volume label and exit"),
        ("--show-defaults", "Show built-in defaults for various tar options"),
        ("-?", "Display a short option summary and exit"),
        ("--usage", "Display a list of available options and exit"),
        ("--version", "Print program version and copyright information and exit"),
    ];
    for (n, d) in ADD {
        if !spec.options.iter().any(|o| o.names.iter().any(|x| x == n)) {
            spec.options.push(opt(n, d));
        }
    }
}

/// systemctl: unit-taking subcommands (`status`, `start`, …) complete unit
/// names upstream via an opaque generator the extractor dropped. Re-add a
/// shell generator listing installed unit files.
fn patch_systemctl(spec: &mut Subcommand) {
    const UNIT_CMDS: &[&str] = &[
        "status", "start", "stop", "restart", "reload", "try-restart", "enable", "disable",
        "reenable", "mask", "unmask", "cat", "show", "is-active", "is-enabled", "is-failed",
        "kill", "clean", "freeze", "thaw", "list-dependencies",
    ];
    for s in &mut spec.subcommands {
        let is_unit_cmd = s
            .names
            .first()
            .map(|n| UNIT_CMDS.contains(&n.as_str()))
            .unwrap_or(false);
        if !is_unit_cmd {
            continue;
        }
        for arg in &mut s.args {
            if arg.generators.is_empty() && arg.templates.is_empty() {
                arg.generators.push(Generator::Script {
                    input: ScriptInput::Shell {
                        script: "systemctl list-unit-files --no-legend --no-pager 2>/dev/null \
                                 | awk '{print $1}' | sort -u"
                            .to_string(),
                    },
                    split_on: Some("\n".to_string()),
                    post_process: PostProcess::Pattern {
                        inner: PostProcessKind::SplitLines {},
                    },
                    timeout_ms: 5000,
                    cache: None,
                });
            }
        }
    }
}

/// yarn: the extractor dropped 6 root options present upstream.
fn patch_yarn(spec: &mut Subcommand) {
    const ADD: &[(&str, &str, bool)] = &[
        ("--cache-folder", "Specify a custom folder to store the yarn cache", true),
        ("--check-files", "Verify file tree of packages for consistency", false),
        ("--cwd", "Working directory to use", true),
        ("--update-checksums", "Update package checksums from current repo", false),
        ("--use-yarnrc", "Specifies a yarnrc file to use", true),
        ("--verbose", "Output verbose messages on internal operations", false),
    ];
    for (n, d, takes_arg) in ADD {
        if spec.options.iter().any(|o| o.names.iter().any(|x| x == n)) {
            continue;
        }
        let mut o = opt(n, d);
        if *takes_arg {
            o.args = vec![Arg::default()];
        }
        spec.options.push(o);
    }
}

/// gh: the long-standing `co` alias (`pr checkout`) is missing from our snapshot.
fn patch_gh(spec: &mut Subcommand) {
    if spec.subcommands.iter().any(|s| s.names.iter().any(|n| n == "co")) {
        return;
    }
    let mut co = sub("co", "Alias for 'pr checkout'");
    co.priority = Some(60);
    spec.subcommands.push(co);
}

/// make: the `target` arg's suggestions come from an opaque JS generator
/// (`cat`/regex over the Makefile) the extractor can't evaluate. Re-add a
/// shell generator that lists targets from a Makefile in the cwd.
fn patch_make(spec: &mut Subcommand) {
    if let Some(arg) = spec.args.first_mut() {
        let empty = arg.generators.is_empty() && arg.templates.is_empty();
        if empty {
            arg.generators.push(Generator::Script {
                input: ScriptInput::Shell {
                    // List targets parsed from a Makefile in the cwd; when none
                    // are found, fall back to the candidate makefile names
                    // (matching upstream's `listTargets`).
                    script: "t=$({ cat GNUmakefile Makefile makefile; } 2>/dev/null | \
                             grep -E '^[a-zA-Z0-9][a-zA-Z0-9_.-]*:' | \
                             sed 's/:.*//' | sort -u); \
                             if [ -n \"$t\" ]; then printf '%s\\n' \"$t\"; \
                             else printf 'GNUmakefile\\nmakefile\\n'; fi"
                        .to_string(),
                },
                split_on: Some("\n".to_string()),
                post_process: PostProcess::Pattern {
                    inner: PostProcessKind::SplitLines {},
                },
                timeout_ms: 5000,
                cache: None,
            });
        }
    }
}

/// asciinema: the extractor dropped `requiresSeparator` on `rec -i`, so our
/// resolver couldn't suppress `-i ` (it needs `-i=<seconds>`). Restore it.
fn patch_asciinema(spec: &mut Subcommand) {
    for s in &mut spec.subcommands {
        if !s.names.iter().any(|n| n == "rec") {
            continue;
        }
        for o in &mut s.options {
            if o.names.iter().any(|n| n == "-i" || n == "--idle-time-limit")
                && o.requires_separator.is_none()
            {
                o.requires_separator = Some("=".to_string());
            }
        }
    }
}

/// cd: upstream completes DIRECTORIES (plus `..`), not files. Our bundled cd
/// uses the filepaths template, so it offers files too and omits `..`.
fn patch_cd(spec: &mut Subcommand) {
    if let Some(arg) = spec.args.first_mut() {
        for g in &mut arg.generators {
            if let Generator::Template { template } = g {
                if *template == Template::Filepaths {
                    *template = Template::Folders;
                }
            }
        }
        arg.templates.retain(|t| *t != Template::Filepaths);
        // `..` is appended by the folders GENERATOR itself (see generator.rs),
        // so no static entry is needed here.
    }
}

/// git: branch-listing generators use `post_process: none` (priority 60 for
/// every branch). Upstream ranks the current branch at 100 and others at 75.
/// Swap those generators to the `GitBranchList` post-process (same cleanup,
/// correct priorities) everywhere a `git branch` listing appears.
fn patch_git(spec: &mut Subcommand) {
    fn walk(sc: &mut Subcommand) {
        for arg in &mut sc.args {
            for g in &mut arg.generators {
                if let Generator::Script {
                    input,
                    post_process,
                    ..
                } = g
                {
                    let lists_branches = match input {
                        ScriptInput::Argv { argv } => argv.iter().any(|a| a == "branch"),
                        ScriptInput::Shell { script } => script.contains(" branch "),
                        ScriptInput::FnTemplate { .. } => false,
                    };
                    if lists_branches && matches!(post_process, PostProcess::None {}) {
                        *post_process = PostProcess::Pattern {
                            inner: PostProcessKind::GitBranchList {},
                        };
                    }
                }
            }
        }
        for s in &mut sc.subcommands {
            walk(s);
        }
    }
    walk(spec);
}

fn sub(name: &str, desc: &str) -> Subcommand {
    Subcommand {
        names: vec![name.to_string()],
        description: Some(desc.to_string()),
        ..Default::default()
    }
}

fn opt(name: &str, desc: &str) -> Opt {
    Opt {
        names: vec![name.to_string()],
        description: Some(desc.to_string()),
        ..Default::default()
    }
}

/// Upstream `terraform ` returns 21 subcommands + 5 dash-options. Our
/// extractor dropped the subcommands (terraform builds them dynamically),
/// leaving only `-help`/`-chdir`/`-version`. Re-stated statically here.
fn terraform() -> Subcommand {
    Subcommand {
        names: vec!["terraform".to_string()],
        description: Some("Infrastructure as code".to_string()),
        subcommands: vec![
            sub("init", "Prepare your working directory for other commands"),
            sub("validate", "Check whether the configuration is valid"),
            sub("plan", "Show changes required by the current configuration"),
            sub("apply", "Create or update infrastructure"),
            sub("destroy", "Destroy previously-created infrastructure"),
            sub("console", "Try Terraform expressions at an interactive command prompt"),
            sub("fmt", "Reformat your configuration in the standard style"),
            sub("force-unlock", "Release a stuck lock on the current workspace"),
            sub("get", "Install or upgrade remote Terraform modules"),
            sub("graph", "Generate a Graphviz graph of the steps in an operation"),
            sub("import", "Associate existing infrastructure with a Terraform resource"),
            sub("login", "Obtain and save credentials for a remote host"),
            sub("logout", "Remove locally-stored credentials for a remote host"),
            sub("output", "Show output values from your root module"),
            sub("providers", "Show the providers required for this configuration"),
            sub("refresh", "Update the state to match remote systems"),
            sub("show", "Show the current state or a saved plan"),
            sub("state", "Advanced state management"),
            sub("taint", "Mark a resource instance as not fully functional"),
            sub("untaint", "Remove the 'tainted' state from a resource instance"),
            sub("workspace", "Workspace management"),
        ],
        options: vec![
            opt("-install-autocomplete", "Install bash/zsh tab completion"),
            opt("-uninstall-autocomplete", "Uninstall bash/zsh tab completion"),
            opt("-help", "Show this help output, or the help for a specified subcommand"),
            opt("-chdir", "Switch to a different working directory before executing"),
            opt("-version", "Show the current Terraform version"),
        ],
        ..Default::default()
    }
}

/// Upstream `kill ` lists running process ids (via `ps`) with the process
/// name as the description. The bundle's kill.json has no pid generator
/// (it was an opaque JS generator our extractor can't evaluate).
fn kill() -> Subcommand {
    let pid_generator = Generator::Script {
        input: ScriptInput::Shell {
            script: "ps -axo pid=,comm= 2>/dev/null".to_string(),
        },
        split_on: Some("\n".to_string()),
        post_process: PostProcess::Pattern {
            inner: PostProcessKind::FirstTokenRest {},
        },
        timeout_ms: 5000,
        cache: None,
    };
    Subcommand {
        names: vec!["kill".to_string()],
        description: Some("Send a signal to a process".to_string()),
        args: vec![Arg {
            name: Some("pid".to_string()),
            is_variadic: true,
            generators: vec![pid_generator],
            ..Default::default()
        }],
        options: vec![
            Opt {
                args: vec![Arg::default()],
                ..opt("-s", "Specify the signal to send")
            },
            opt("-l", "List signal names"),
        ],
        ..Default::default()
    }
}
