use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use insh_rs::{
    commands, config::UiMode, env as insh_env, pty, resources, shell::Shell, shell_init,
};

/// IDE-style shell autocomplete in Rust. Drop-in compatible with Microsoft's
/// inshellisense (`is`) — insh-rs ships both `insh` and `is` binaries, reads
/// the same `~/.inshellisenserc` / `~/.config/inshellisense/rc.toml`, honors
/// the same `ISTERM` / `ISTERM_LOGIN` / `ISTERM_TESTING` env vars, and emits
/// the same OSC 6973 prompt markers.
#[derive(Parser)]
#[command(
    name = "insh",
    bin_name = "insh",
    version,
    about = "IDE-style shell autocomplete in Rust",
    long_about = None,
    disable_version_flag = true,
)]
struct Cli {
    /// Print the current version and exit.
    #[arg(short = 'v', long, global = true)]
    version: bool,

    /// Start the wrapped shell as a login shell.
    #[arg(short = 'l', long, global = true)]
    login: bool,

    /// Shell to use (bash, zsh, fish, pwsh, powershell, xonsh, nu). Default
    /// is auto-detected from the current environment.
    #[arg(short = 's', long, global = true, value_enum)]
    shell: Option<Shell>,

    /// Check whether the current process is running inside an insh-rs
    /// session; prints a one-line status and exits 0 (live) or 1 (not found).
    #[arg(short = 'c', long, global = true)]
    check: bool,

    /// Enable verbose diagnostic output.
    #[arg(short = 'V', long, global = true)]
    verbose: bool,

    /// Deterministic test mode (sets ISTERM_TESTING, used by e2e tests).
    #[arg(short = 'T', long, global = true, hide = true)]
    test: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start an interactive shell wrapped with autocomplete
    /// (default when no subcommand is given). UI defaults to ghost
    /// text; use `--ui popup` for the upstream-style popup TUI.
    Start {
        /// Override the suggestion UI mode. Defaults to the config
        /// file's `ui` field (which defaults to `ghost`).
        #[arg(long, value_enum)]
        ui: Option<UiMode>,
    },

    /// Print or install the init snippet for the given shell.
    Init {
        /// Which shell to emit the init snippet for.
        #[arg(value_enum)]
        shell: Option<Shell>,
        /// Instead of printing the snippet, append it to the shell's rc file.
        #[arg(long = "install-rc")]
        install_rc: bool,
    },

    /// Regenerate all shell init files and re-unpack resources.
    Reinit,

    /// Convenience alias for `insh init --install-rc` — appends the bash
    /// init snippet to ~/.bashrc. Kept from phase 0 for backwards compat.
    #[command(hide = true)]
    Install,

    /// Run health checks and print resolved configuration.
    Doctor,

    /// Offline completion query: prints a suggestion for the given line.
    Complete(CompleteArgs),

    /// Manage loaded completion specs.
    #[command(subcommand)]
    Specs(SpecsCmd),

    /// Deprecated alias for `specs list`. Use `insh specs list` instead.
    #[command(hide = true, alias = "listspecs")]
    ListSpecs,

    /// Remove cached resources (preserves user config).
    Uninstall,
}

#[derive(Args)]
struct CompleteArgs {
    /// The command line so far (what the user has typed).
    line: String,
    /// Emit the full ranked Vec<Suggestion> as JSON instead of just the
    /// top tail. Useful for inspection and parity testing.
    #[arg(long)]
    json: bool,
    /// Override cwd (defaults to ".")
    #[arg(long, default_value = ".")]
    cwd: String,
}

#[derive(Subcommand)]
enum SpecsCmd {
    /// List the names of all loaded specs as a JSON array to stdout.
    List {
        /// Emit one name per line instead of a JSON array.
        #[arg(long)]
        plain: bool,
    },
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();

    // Top-level --version and --check short-circuit before dispatch.
    if cli.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if cli.check {
        if insh_env::session_active() {
            println!("insh-rs session live");
            return Ok(());
        }
        println!("insh-rs session not found");
        std::process::exit(1);
    }

    match cli.command.unwrap_or(Cmd::Start { ui: None }) {
        Cmd::Start { ui } => {
            // Eagerly unpack resources so the wrapped shell can find its
            // integration scripts under ~/.insh-rs/. Cheap: the unpacker
            // is version-gated and no-ops on second run.
            let _ = resources::unpack();
            let cfg = insh_rs::config::load();
            let effective_ui = ui.unwrap_or(cfg.ui);
            if cli.verbose {
                eprintln!("insh-rs: ui mode = {}", effective_ui.as_str());
            }
            pty::run_wrapped_shell()
        }
        Cmd::Init { shell, install_rc } => {
            let target = shell.unwrap_or(Shell::Bash);
            let _ = resources::unpack();
            if install_rc {
                shell_init::install()
            } else {
                shell_init::print_init(target.as_str())
            }
        }
        Cmd::Reinit => commands::reinit::run(),
        Cmd::Install => {
            let _ = resources::unpack();
            shell_init::install()
        }
        Cmd::Doctor => commands::doctor::run(),
        Cmd::Complete(CompleteArgs { line, json, cwd }) => {
            commands::complete::run(&line, json, &cwd)
        }
        Cmd::Specs(SpecsCmd::List { plain }) => commands::specs::list(plain),
        Cmd::ListSpecs => {
            eprintln!("insh-rs: `list-specs` is deprecated; use `insh specs list` instead");
            commands::specs::list(true)
        }
        Cmd::Uninstall => commands::uninstall::run(),
    }
}
