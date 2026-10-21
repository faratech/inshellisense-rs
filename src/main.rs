use anyhow::{Context, Result};
use inshellisense_rs::{
    commands, config::UiMode, env as is_env, pty, resources, shell::Shell, shell_init,
};
use std::ffi::OsString;

pub fn main() -> Result<()> {
    let args = collect_args(std::env::args_os());

    // Parse root flags that appear before the subcommand. Subcommands parse
    // their own documented flags after dispatch so completion lines can still
    // contain arbitrary shell-looking text when quoted.
    let mut version = false;
    let mut login = false;
    let mut shell: Option<Shell> = None;
    let mut check = false;
    let mut verbose = false;
    let mut test = false;
    let mut rest: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--version" => version = true,
            "-l" | "--login" => login = true,
            "-c" | "--check" => check = true,
            "-V" | "--verbose" => verbose = true,
            "-T" | "--test" => test = true,
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-s" | "--shell" => {
                i += 1;
                if i < args.len() {
                    shell = Some(parse_shell_value(&args[i])?);
                } else {
                    anyhow::bail!("missing value for {}", args[i - 1]);
                }
            }
            other => {
                // Might be -s<value> (no space).
                if let Some(val) = other.strip_prefix("-s") {
                    shell = Some(parse_shell_value(val)?);
                } else if let Some(val) = other.strip_prefix("--shell=") {
                    shell = Some(parse_shell_value(val)?);
                } else {
                    rest.push(args[i].clone());
                    // Collect all remaining args as subcommand args.
                    rest.extend_from_slice(&args[i + 1..]);
                    break;
                }
            }
        }
        i += 1;
    }

    // Top-level --version and --check short-circuit.
    if version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if check {
        if is_env::session_active() {
            println!("inshellisense session [live]");
            return Ok(());
        }
        // Upstream exits 0 here (it reports status, not an error).
        println!("inshellisense session [not found]");
        return Ok(());
    }

    // Dispatch on subcommand (first non-flag arg). Default = start.
    let subcmd = rest.first().map(|s| s.as_str()).unwrap_or("start");
    match subcmd {
        "start" => {
            if has_help_flag(&rest) {
                print_start_help();
                return Ok(());
            }
            // Re-entry guard: if already inside an inshellisense
            // session, print confirmation and exit — don't nest.
            // Matches upstream's behavior at commands/root.ts:25-29.
            if is_env::session_active() {
                println!("inshellisense session [live]");
                return Ok(());
            }
            let start_opts = parse_start_args(&rest, shell, login, verbose, test)?;
            let shell = start_opts
                .shell
                .unwrap_or_else(inshellisense_rs::shell::detect);
            if start_opts.verbose {
                let cfg = inshellisense_rs::config::load();
                let effective_ui = start_opts.ui.unwrap_or(cfg.ui);
                eprintln!("inshellisense-rs: ui = {}", effective_ui.as_str());
                eprintln!("inshellisense-rs: shell = {}", shell.as_str());
                eprintln!("inshellisense-rs: login = {}", start_opts.login);
            }
            pty::run_wrapped(shell, start_opts.login, start_opts.ui, start_opts.test)
        }
        "init" => {
            if has_help_flag(&rest) {
                print_init_help();
                return Ok(());
            }
            let init_opts = parse_init_args(&rest, shell)?;
            let target = init_opts.shell.unwrap_or(Shell::Bash);
            resources::unpack()?;
            if init_opts.install_rc {
                shell_init::install_rc(target)
            } else {
                shell_init::print_init(target.as_str())
            }
        }
        "reinit" => {
            if has_help_flag(&rest) {
                print_reinit_help();
                return Ok(());
            }
            reject_unknown_args("reinit", &rest, &["-h", "--help"], &[])?;
            commands::reinit::run()
        }
        "install" => {
            if has_help_flag(&rest) {
                print_install_help();
                return Ok(());
            }
            // `install` only ever writes the bash auto-exec wrapper. Accepting
            // and ignoring `--shell zsh` would silently edit `.bashrc` instead.
            let names_a_shell = shell.is_some()
                || rest
                    .iter()
                    .any(|a| a == "-s" || a == "--shell" || a.starts_with("--shell="));
            if names_a_shell {
                anyhow::bail!(
                    "is install: `--shell` is not supported; run `is init <shell> --install-rc` instead"
                );
            }
            reject_unknown_args("install", &rest, &["-h", "--help"], &[])?;
            resources::unpack()?;
            shell_init::install()
        }
        "doctor" => {
            if has_help_flag(&rest) {
                print_doctor_help();
                return Ok(());
            }
            reject_unknown_args("doctor", &rest, &["-h", "--help"], &[])?;
            commands::doctor::run()
        }
        "complete" => {
            if has_help_flag(&rest) {
                print_complete_help();
                return Ok(());
            }
            // Default output is JSON (matching upstream). --text
            // switches to plain ghost-tail mode. --json accepted
            // as a no-op for backwards compat.
            let opts = parse_complete_args(&rest)?;
            // `is complete "ls " --shell pwsh` — the shell flag may appear
            // after the subcommand, where the root parser never sees it.
            // It used to be appended to the completion line instead.
            let shell = opts.shell.or(shell);
            commands::complete::run(&opts.line, opts.text_mode, &opts.cwd, shell)
        }
        "specs" => {
            let sub2 = rest.get(1).map(|s| s.as_str()).unwrap_or("list");
            match sub2 {
                "-h" | "--help" | "help" => {
                    // `is specs help|--help` → specs help.
                    // `is specs help|--help <cmd>` → per-subcommand help, or a
                    // clear error for an unknown subcommand (GH issue #1). The
                    // `help` and `--help` forms behave identically.
                    match rest.get(2).map(|s| s.as_str()) {
                        None => print_specs_help(),
                        Some("list") => print_specs_list_help(),
                        Some("help") => print_specs_help(),
                        Some(other) => {
                            eprintln!("is specs: unknown subcommand `{}`", other);
                            std::process::exit(2);
                        }
                    }
                    Ok(())
                }
                "list" => {
                    if has_help_flag(&rest[1..]) {
                        print_specs_list_help();
                        return Ok(());
                    }
                    reject_unknown_args(
                        "specs list",
                        &rest[1..],
                        &["-h", "--help", "--plain", "--shell", "-s"],
                        &["--shell", "-s"],
                    )?;
                    let plain = rest.iter().any(|a| a == "--plain");
                    let specs_shell = parse_optional_shell_flag(&rest, "--shell")?;
                    commands::specs::list(plain, specs_shell)
                }
                other => {
                    eprintln!("is specs: unknown subcommand `{}`", other);
                    std::process::exit(2);
                }
            }
        }
        "list-specs" | "listspecs" => {
            if has_help_flag(&rest) {
                print_specs_list_help();
                return Ok(());
            }
            eprintln!("inshellisense-rs: `list-specs` is deprecated; use `is specs list` instead");
            reject_unknown_args("list-specs", &rest, &["-h", "--help"], &[])?;
            commands::specs::list(true, None)
        }
        "uninstall" => {
            if has_help_flag(&rest) {
                print_uninstall_help();
                return Ok(());
            }
            // Guard hardest here: an ignored `--dry-run` used to delete files.
            reject_unknown_args("uninstall", &rest, &["-h", "--help"], &[])?;
            commands::uninstall::run()
        }
        other => {
            eprintln!(
                "is: unknown command `{}`\nRun `is --help` for usage.",
                other
            );
            std::process::exit(2);
        }
    }
}

/// Collect argv as `String`s, replacing bytes that are not valid UTF-8 with
/// the replacement character.
///
/// `env::args()` panics on such an argument (`is complete "$(printf
/// 'caf\xe9')"` exited 101 with no output at all), which broke any script
/// consuming the JSON. Completion lines legitimately contain arbitrary shell
/// text — filenames in latin-1 among it — so degrade lossily instead of
/// aborting before a single argument has been parsed.
fn collect_args<I>(argv: I) -> Vec<String>
where
    I: IntoIterator<Item = OsString>,
{
    argv.into_iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn parse_shell(s: &str) -> Option<Shell> {
    match s.to_lowercase().as_str() {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        "pwsh" => Some(Shell::Pwsh),
        "powershell" => Some(Shell::Powershell),
        "xonsh" => Some(Shell::Xonsh),
        "nu" => Some(Shell::Nu),
        #[cfg(windows)]
        "cmd" => Some(Shell::Cmd),
        _ => None,
    }
}

fn parse_shell_value(s: &str) -> Result<Shell> {
    parse_shell(s).with_context(|| {
        format!(
            "Unsupported shell: '{}', supported shells: bash, zsh, fish, pwsh, powershell, xonsh, nu",
            s
        )
    })
}

fn parse_ui(s: &str) -> Option<UiMode> {
    match s.to_lowercase().as_str() {
        "ghost" => Some(UiMode::Ghost),
        "popup" => Some(UiMode::Popup),
        "hybrid" => Some(UiMode::Hybrid),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct StartArgs {
    shell: Option<Shell>,
    login: bool,
    verbose: bool,
    test: bool,
    ui: Option<UiMode>,
}

fn parse_start_args(
    args: &[String],
    root_shell: Option<Shell>,
    root_login: bool,
    root_verbose: bool,
    root_test: bool,
) -> Result<StartArgs> {
    let mut out = StartArgs {
        shell: root_shell,
        login: root_login,
        verbose: root_verbose,
        test: root_test,
        ui: None,
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-l" | "--login" => out.login = true,
            "-V" | "--verbose" => out.verbose = true,
            "-T" | "--test" => out.test = true,
            "-s" | "--shell" => {
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| format!("missing value for {}", args[i - 1]))?;
                out.shell = Some(parse_shell_value(value)?);
            }
            "--ui" => {
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| format!("missing value for {}", args[i - 1]))?;
                out.ui = Some(parse_ui_value(value)?);
            }
            other => {
                if let Some(value) = other.strip_prefix("--shell=") {
                    out.shell = Some(parse_shell_value(value)?);
                } else if let Some(value) = other.strip_prefix("-s") {
                    out.shell = Some(parse_shell_value(value)?);
                } else if let Some(value) = other.strip_prefix("--ui=") {
                    out.ui = Some(parse_ui_value(value)?);
                } else {
                    anyhow::bail!("is start: unknown option `{}`", other);
                }
            }
        }
        i += 1;
    }
    Ok(out)
}

fn parse_ui_value(s: &str) -> Result<UiMode> {
    parse_ui(s).with_context(|| {
        format!(
            "Unsupported ui mode: '{}', supported modes: ghost, popup, hybrid",
            s
        )
    })
}

#[derive(Debug, Clone)]
struct InitArgs {
    shell: Option<Shell>,
    install_rc: bool,
}

fn parse_init_args(args: &[String], root_shell: Option<Shell>) -> Result<InitArgs> {
    let mut shell = root_shell;
    let mut install_rc = false;
    let mut positional_shell: Option<Shell> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--install-rc" => install_rc = true,
            "-s" | "--shell" => {
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| format!("missing value for {}", args[i - 1]))?;
                shell = Some(parse_shell_value(value)?);
            }
            other => {
                if let Some(value) = other.strip_prefix("--shell=") {
                    shell = Some(parse_shell_value(value)?);
                } else if let Some(value) = other.strip_prefix("-s") {
                    shell = Some(parse_shell_value(value)?);
                } else if other.starts_with('-') {
                    anyhow::bail!("is init: unknown option `{}`", other);
                } else if positional_shell.is_none() {
                    positional_shell = Some(parse_shell_value(other)?);
                } else {
                    anyhow::bail!("is init: unexpected argument `{}`", other);
                }
            }
        }
        i += 1;
    }
    if positional_shell.is_some() {
        shell = positional_shell;
    }
    Ok(InitArgs { shell, install_rc })
}

#[derive(Debug, Clone)]
struct CompleteArgs {
    shell: Option<Shell>,
    text_mode: bool,
    cwd: String,
    line: String,
}

fn parse_complete_args(args: &[String]) -> Result<CompleteArgs> {
    let mut shell = None;
    let mut text_mode = false;
    let mut cwd = ".".to_string();
    let mut line_parts: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                line_parts.extend_from_slice(&args[i + 1..]);
                break;
            }
            "--text" => text_mode = true,
            "--json" => {}
            "--cwd" | "--shell" | "-s" => {
                let flag = args[i].clone();
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| format!("missing value for {flag}"))?
                    .clone();
                if flag == "--cwd" {
                    cwd = value;
                } else {
                    shell = Some(parse_shell_value(&value)?);
                }
            }
            other => {
                if let Some(value) = other.strip_prefix("--cwd=") {
                    cwd = value.to_string();
                } else if let Some(value) = other.strip_prefix("--shell=") {
                    shell = Some(parse_shell_value(value)?);
                } else if let Some(value) = other.strip_prefix("-s") {
                    shell = Some(parse_shell_value(value)?);
                } else {
                    line_parts.push(other.to_string());
                }
            }
        }
        i += 1;
    }
    Ok(CompleteArgs {
        shell,
        text_mode,
        cwd,
        line: line_parts.join(" "),
    })
}

/// Parse `--shell <v>` / `--shell=<v>`, plus upstream's `-s <v>` / `-s<v>`.
fn parse_optional_shell_flag(args: &[String], flag: &str) -> Result<Option<Shell>> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == flag || a == "-s" {
            let value = it
                .next()
                .with_context(|| format!("missing value for {}", a))?;
            return Ok(Some(parse_shell_value(value)?));
        }
        if let Some(val) = a.strip_prefix(&format!("{}=", flag)) {
            return Ok(Some(parse_shell_value(val)?));
        }
        if let Some(val) = a.strip_prefix("-s")
            && !val.is_empty()
        {
            return Ok(Some(parse_shell_value(val)?));
        }
    }
    Ok(None)
}

/// Reject any argument a subcommand does not understand.
///
/// Subcommands used to drop unrecognized arguments on the floor, so
/// `is uninstall --dry-run` performed a real uninstall and `is install
/// --shell zsh` wrote to `.bashrc`. A flag we do not implement must never be
/// mistaken for one we honor, least of all on a destructive command.
/// `value_flags` name the allowed flags that consume the following argument,
/// so `--shell zsh` does not report `zsh` as unknown.
fn reject_unknown_args(
    cmd: &str,
    args: &[String],
    allowed: &[&str],
    value_flags: &[&str],
) -> Result<()> {
    let mut skip_value = false;
    for arg in args.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        // Attached value: `--flag=value` is only valid for flags that take a
        // value. Splitting at `=` and checking the name alone let
        // `--help=foo` pass as `--help` on every guarded command — including
        // `uninstall`, which then ran (#92).
        if let Some((name, _)) = arg.split_once('=') {
            if !value_flags.contains(&name) {
                anyhow::bail!("is {cmd}: unknown argument `{arg}`");
            }
            continue;
        }
        // Attached short value: `-szsh` is `-s zsh` wherever `-s` takes a
        // value (other subcommands accept this form; #112).
        if allowed.contains(&"-s")
            && value_flags.contains(&"-s")
            && arg.starts_with("-s")
            && arg.len() > 2
        {
            continue;
        }
        if !allowed.contains(&arg.as_str()) {
            anyhow::bail!("is {cmd}: unknown argument `{arg}`");
        }
        skip_value = value_flags.contains(&arg.as_str());
    }
    Ok(())
}

/// Recognize help before `--`, excluding values consumed by known flags.
/// The first element is the subcommand name, so only later args are flags.
fn has_help_flag(args: &[String]) -> bool {
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => break,
            "--cwd" | "--shell" | "-s" | "--ui" => {
                args.next();
            }
            "-h" | "--help" => return true,
            _ => {}
        }
    }
    false
}

fn print_help() {
    println!(
        "IDE-style shell autocomplete in Rust

Usage: is [OPTIONS] [COMMAND]

Commands:
  start      Start an interactive shell wrapped with autocomplete (default)
  init       Print or install the init snippet for the given shell
  reinit     Regenerate all shell init files and re-unpack resources
  doctor     Run health checks and print resolved configuration
  complete   Offline completion query: prints a suggestion for the given line
  specs      Manage loaded completion specs
  uninstall  Remove cached resources (preserves user config)

Options:
  -v, --version        Print the current version and exit
  -l, --login          Start the wrapped shell as a login shell
  -s, --shell <SHELL>  Shell to use (bash, zsh, fish, pwsh, powershell, xonsh, nu)
  -c, --check          Check whether running inside an inshellisense-rs session
  -V, --verbose        Enable verbose diagnostic output
  -h, --help           Print this help"
    );
}

fn print_start_help() {
    println!(
        "Start an interactive shell wrapped with autocomplete (default subcommand)

Usage: is start [OPTIONS]

Options:
  --ui <MODE>          UI mode: ghost, popup, or hybrid (default: hybrid)
  -s, --shell <SHELL>  Shell to wrap (bash, zsh, fish, pwsh, powershell, xonsh, nu)
  -l, --login          Start the wrapped shell as a login shell
  -V, --verbose        Print the resolved ui/shell/login configuration on startup
  -h, --help           Print this help"
    );
}

fn print_init_help() {
    println!(
        "Print or install the init snippet for the given shell

Usage: is init [OPTIONS] [SHELL]

Arguments:
  [SHELL]              Target shell: bash, zsh, fish, pwsh, powershell, xonsh, nu

Options:
  --install-rc         Install the init snippet into the user's shell rc file
  -s, --shell <SHELL>  Equivalent to passing SHELL positionally
  -h, --help           Print this help"
    );
}

fn print_reinit_help() {
    println!(
        "Regenerate all shell init files and re-unpack bundled resources

Usage: is reinit

Options:
  -h, --help  Print this help"
    );
}

fn print_install_help() {
    println!(
        "Install the init snippet into the user's shell rc file

Usage: is install

Options:
  -h, --help  Print this help"
    );
}

fn print_doctor_help() {
    println!(
        "Run health checks and print resolved configuration

Usage: is doctor

Options:
  -h, --help  Print this help"
    );
}

fn print_complete_help() {
    println!(
        "Offline completion query: prints a suggestion for the given line

Usage: is complete [OPTIONS] <LINE>

Arguments:
  <LINE>          Command line to complete (quote it if it contains spaces)

Options:
  --text          Print only the ghost-tail text instead of the default JSON
  --json          Accepted as a no-op for backwards compatibility
  --cwd <DIR>     Resolve files/dirs relative to DIR (default: current directory)
  -s, --shell <SHELL>  Use shell-specific parsing and completion
  --              Treat remaining arguments as literal completion text
  -h, --help      Print this help"
    );
}

fn print_uninstall_help() {
    println!(
        "Remove cached resources (preserves user config)

Usage: is uninstall

Options:
  -h, --help  Print this help"
    );
}

fn print_specs_help() {
    println!(
        "Manage loaded completion specs

Usage: is specs [OPTIONS] [COMMAND]

Commands:
  list [options]  List the names of all available specs
  help [command]  Print help for a specs subcommand

Options:
  -h, --help  Print this help"
    );
}

fn print_specs_list_help() {
    println!(
        "List the names of all available specs

Usage: is specs list [OPTIONS]

Options:
  --plain              Print one spec name per line (no decoration)
  -s, --shell <SHELL>  Filter to specs relevant for the given shell
  -h, --help           Print this help"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_shell_forms_are_consumed_once() {
        for flags in [
            vec!["-spwsh"],
            vec!["-s", "pwsh"],
            vec!["--shell=pwsh"],
            vec!["--shell", "pwsh"],
        ] {
            let args = [vec!["complete"], flags, vec!["ls -"]]
                .concat()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let options = parse_complete_args(&args).unwrap();
            assert_eq!(options.shell, Some(Shell::Pwsh));
            assert_eq!(options.line, "ls -");
        }
    }

    #[test]
    fn completion_delimiter_keeps_literal_flags_in_the_line() {
        let args = [
            "complete",
            "-spwsh",
            "--",
            "ls -",
            "--shell",
            "not-a-shell",
            "--help",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let options = parse_complete_args(&args).unwrap();
        assert_eq!(options.shell, Some(Shell::Pwsh));
        assert_eq!(options.line, "ls - --shell not-a-shell --help");
        assert!(!has_help_flag(&args));
        let literal = ["complete", "--", "--shell", "not-a-shell", "-h"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            parse_optional_shell_flag(&literal, "--shell").unwrap(),
            None
        );
        assert!(!has_help_flag(&literal));
    }

    #[test]
    fn help_detection_skips_option_values() {
        let args = ["complete", "--cwd", "--help", "ls"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(!has_help_flag(&args));
        let args = ["complete", "--text", "--help"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(has_help_flag(&args));
    }

    #[test]
    fn collect_args_skips_the_program_name() {
        let argv = vec![
            OsString::from("is"),
            OsString::from("complete"),
            OsString::from("git ch"),
        ];
        assert_eq!(collect_args(argv), vec!["complete", "git ch"]);
    }

    /// #92: an attached value must not turn a valueless flag into a pass.
    /// `--help=foo` used to satisfy the `--help` allowlist entry while never
    /// triggering help, so guarded commands (including `uninstall`) ran.
    #[test]
    fn reject_unknown_args_rejects_attached_value_on_valueless_flag() {
        let args = vec!["uninstall".to_string(), "--help=foo".to_string()];
        assert!(reject_unknown_args("uninstall", &args, &["-h", "--help"], &[]).is_err());
        let ok = vec!["doctor".to_string(), "--help".to_string()];
        assert!(reject_unknown_args("doctor", &ok, &["-h", "--help"], &[]).is_ok());
    }

    /// #112: `-szsh` is the attached form of a value flag and must pass
    /// wherever `-s zsh` does; `--shell=zsh` likewise.
    #[test]
    fn reject_unknown_args_accepts_attached_value_flags() {
        let allowed = &["-h", "--help", "--plain", "--shell", "-s"];
        let values = &["--shell", "-s"];
        for form in ["-szsh", "--shell=zsh", "-s", "--shell"] {
            let mut args = vec!["list".to_string(), form.to_string()];
            if form == "-s" || form == "--shell" {
                args.push("zsh".to_string());
            }
            assert!(
                reject_unknown_args("specs list", &args, allowed, values).is_ok(),
                "{form} should be accepted"
            );
        }
    }

    /// A filename in latin-1 or quoted shell text can carry bytes that are
    /// not valid UTF-8. `env::args()` panicked on those (exit 101, no stdout);
    /// the lossy conversion must keep every argument and degrade only the
    /// invalid bytes.
    #[test]
    fn collect_args_survives_non_utf8_bytes() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let argv = vec![
                OsString::from("is"),
                OsString::from("complete"),
                OsString::from_vec(vec![0x63, 0x61, 0x66, 0xe9]),
            ];
            let args = collect_args(argv);
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "complete");
            assert_eq!(args[1], "caf\u{FFFD}");
        }
        #[cfg(not(unix))]
        {
            let args = collect_args(vec![OsString::from("is"), OsString::from("complete")]);
            assert_eq!(args, vec!["complete"]);
        }
    }
}
