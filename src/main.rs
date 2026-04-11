use anyhow::Result;
use clap::{Parser, Subcommand};
use insh_rs::{history, pty, shell_init, spec, suggest};

#[derive(Parser)]
#[command(name = "insh", version, about = "IDE-style shell autocomplete in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start an interactive shell wrapped with ghost-text autocomplete (default)
    Start,
    /// Print the init snippet to source from your .bashrc
    Init {
        #[arg(long, default_value = "bash")]
        shell: String,
    },
    /// Install init snippet into your shell rc file
    Install,
    /// Run health checks and show resolved configuration
    Doctor,
    /// Offline completion query: prints a suggestion for the given command line
    Complete {
        /// The command line so far (what the user has typed)
        line: String,
        /// Emit the full ranked Vec<Suggestion> as JSON instead of just the
        /// top tail. Useful for inspection and parity testing.
        #[arg(long)]
        json: bool,
        /// Override cwd (defaults to ".")
        #[arg(long, default_value = ".")]
        cwd: String,
    },
    /// List loaded specs (curated + TOML + JS)
    ListSpecs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Start) {
        Cmd::Start => pty::run_wrapped_shell(),
        Cmd::Init { shell } => shell_init::print_init(&shell),
        Cmd::Install => shell_init::install(),
        Cmd::Doctor => doctor(),
        Cmd::Complete { line, json, cwd } => complete_once(&line, json, &cwd),
        Cmd::ListSpecs => list_specs(),
    }
}

fn doctor() -> Result<()> {
    println!("insh-rs doctor");
    println!("  bash present: {}", which("bash"));
    println!("  HOME: {:?}", dirs::home_dir());
    println!("  bash history entries: {}", history::load().len());
    let specs = spec::Registry::new_with_defaults();
    println!("  curated specs loaded: {}", specs.len());
    #[cfg(feature = "js")]
    println!("  js runtime: boa_engine (enabled)");
    #[cfg(not(feature = "js"))]
    println!("  js runtime: disabled (build with --features js)");
    Ok(())
}

fn complete_once(line: &str, json: bool, cwd: &str) -> Result<()> {
    let hist = history::load();
    let registry = spec::Registry::new_with_defaults();
    let engine = suggest::Engine::new(registry, hist);
    if json {
        let blob = engine.suggest_blob(line, cwd);
        // Use serde_json for structured output.
        let printable: Vec<serde_json::Value> = blob
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "display_name": s.display_name,
                    "insert_value": s.insert_value,
                    "description": s.description,
                    "icon": s.icon,
                    "type": format!("{:?}", s.suggestion_type).to_lowercase(),
                    "priority": s.priority,
                    "hidden": s.hidden,
                    "deprecated": s.deprecated,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&printable)?);
        return Ok(());
    }
    if let Some(s) = engine.suggest(line, cwd) {
        println!("{}", s);
    }
    Ok(())
}

fn list_specs() -> Result<()> {
    let registry = spec::Registry::new_with_defaults();
    for name in registry.names() {
        println!("{}", name);
    }
    Ok(())
}

fn which(cmd: &str) -> bool {
    std::env::var("PATH")
        .ok()
        .map(|p| {
            p.split(':').any(|dir| {
                std::path::Path::new(dir).join(cmd).exists()
            })
        })
        .unwrap_or(false)
}
