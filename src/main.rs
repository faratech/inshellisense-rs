use anyhow::Result;
use inshellisense_rs::{
    commands, config::UiMode, env as is_env, pty, resources, shell::Shell, shell_init,
};

pub fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse root flags (can appear before or after the subcommand).
    let mut version = false;
    let mut login = false;
    let mut shell: Option<Shell> = None;
    let mut check = false;
    let mut verbose = false;
    let mut rest: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--version" => version = true,
            "-l" | "--login" => login = true,
            "-c" | "--check" => check = true,
            "-V" | "--verbose" => verbose = true,
            "-T" | "--test" => {} // accepted, ignored (legacy)
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-s" | "--shell" => {
                i += 1;
                if i < args.len() {
                    shell = parse_shell(&args[i]);
                }
            }
            other => {
                // Might be -s<value> (no space).
                if let Some(val) = other.strip_prefix("-s") {
                    shell = parse_shell(val);
                } else if let Some(val) = other.strip_prefix("--shell=") {
                    shell = parse_shell(val);
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
            println!("inshellisense-rs session live");
            return Ok(());
        }
        println!("inshellisense-rs session not found");
        std::process::exit(1);
    }

    // Dispatch on subcommand (first non-flag arg). Default = start.
    let subcmd = rest.first().map(|s| s.as_str()).unwrap_or("start");
    match subcmd {
        "start" => {
            // Re-entry guard: if already inside an inshellisense
            // session, print confirmation and exit — don't nest.
            // Matches upstream's behavior at commands/root.ts:25-29.
            if is_env::session_active() {
                println!("inshellisense-rs session live");
                return Ok(());
            }
            let ui = parse_subcmd_flag(&rest, "--ui").and_then(|v| parse_ui(&v));
            let _ = resources::unpack();
            let shell = shell.unwrap_or_else(inshellisense_rs::shell::detect);
            if verbose {
                let cfg = inshellisense_rs::config::load();
                let effective_ui = ui.unwrap_or(cfg.ui);
                eprintln!("inshellisense-rs: ui = {}", effective_ui.as_str());
                eprintln!("inshellisense-rs: shell = {}", shell.as_str());
                eprintln!("inshellisense-rs: login = {}", login);
            }
            pty::run_wrapped(shell, login, ui)
        }
        "init" => {
            let install_rc = rest.iter().any(|a| a == "--install-rc");
            let target = rest
                .iter()
                .skip(1)
                .find(|a| !a.starts_with('-'))
                .and_then(|s| parse_shell(s))
                .or(shell)
                .unwrap_or(Shell::Bash);
            let _ = resources::unpack();
            if install_rc {
                shell_init::install()
            } else {
                shell_init::print_init(target.as_str())
            }
        }
        "reinit" => commands::reinit::run(),
        "install" => {
            let _ = resources::unpack();
            shell_init::install()
        }
        "doctor" => commands::doctor::run(),
        "complete" => {
            // Default output is JSON (matching upstream). --text
            // switches to plain ghost-tail mode. --json accepted
            // as a no-op for backwards compat.
            let text_mode = rest.iter().any(|a| a == "--text");
            let cwd = parse_subcmd_flag(&rest, "--cwd").unwrap_or_else(|| ".".to_string());
            let line = rest
                .iter()
                .skip(1)
                .find(|a| !a.starts_with('-'))
                .cloned()
                .unwrap_or_default();
            commands::complete::run(&line, text_mode, &cwd)
        }
        "specs" => {
            let sub2 = rest.get(1).map(|s| s.as_str()).unwrap_or("list");
            match sub2 {
                "list" => {
                    let plain = rest.iter().any(|a| a == "--plain");
                    commands::specs::list(plain)
                }
                other => {
                    eprintln!("is specs: unknown subcommand `{}`", other);
                    std::process::exit(2);
                }
            }
        }
        "list-specs" | "listspecs" => {
            eprintln!("inshellisense-rs: `list-specs` is deprecated; use `is specs list` instead");
            commands::specs::list(true)
        }
        "uninstall" => commands::uninstall::run(),
        other => {
            eprintln!("is: unknown command `{}`\nRun `is --help` for usage.", other);
            std::process::exit(2);
        }
    }
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

fn parse_ui(s: &str) -> Option<UiMode> {
    match s.to_lowercase().as_str() {
        "ghost" => Some(UiMode::Ghost),
        "popup" => Some(UiMode::Popup),
        "hybrid" => Some(UiMode::Hybrid),
        _ => None,
    }
}

/// Extract `--flag value` or `--flag=value` from a subcommand's args.
fn parse_subcmd_flag(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(val) = a.strip_prefix(&format!("{}=", flag)) {
            return Some(val.to_string());
        }
    }
    None
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
