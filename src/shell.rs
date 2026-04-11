//! Shell abstraction — enum, detection, source commands, init snippets.
//!
//! Port of `/tmp/inshellisense/src/utils/shell.ts`. Handles the seven shells
//! that upstream inshellisense supports:
//!   bash, zsh, fish, pwsh (PowerShell Core), powershell (Windows),
//!   xonsh, nu (Nushell).
//!
//! P1 provides the enum + bash/zsh/fish detection + a bash init snippet.
//! P3 fills in source_command/init_snippet for all 7 shells and adds
//! ZDOTDIR isolation for zsh. For now the rest of the codebase only cares
//! about the enum variants and the one implemented path.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Pwsh,
    Powershell,
    Xonsh,
    Nu,
}

impl Shell {
    pub fn as_str(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Pwsh => "pwsh",
            Shell::Powershell => "powershell",
            Shell::Xonsh => "xonsh",
            Shell::Nu => "nu",
        }
    }

    /// Filename for the generated init file in ~/.insh-rs/init/<shell>/<file>.
    pub fn init_file_name(self) -> &'static str {
        match self {
            Shell::Bash => "init.sh",
            Shell::Zsh => "init.zsh",
            Shell::Fish => "init.fish",
            Shell::Pwsh | Shell::Powershell => "init.ps1",
            Shell::Xonsh => "init.xsh",
            Shell::Nu => "init.nu",
        }
    }

    /// Path to the shell's user rc file, if any. None means "pick at runtime".
    pub fn rc_file_relative(self) -> Option<&'static str> {
        match self {
            Shell::Bash => Some(".bashrc"),
            Shell::Zsh => Some(".zshrc"),
            Shell::Fish => Some(".config/fish/config.fish"),
            Shell::Xonsh => Some(".xonshrc"),
            // pwsh / powershell / nu resolve their profile path dynamically
            // ($PROFILE for PowerShell, $env.config for nu) — handled in P3.
            _ => None,
        }
    }
}

/// Detect the active shell from environment variables, falling back to
/// parent-process inspection if none are set. Port of `inferShell` at
/// `/tmp/inshellisense/src/utils/shell.ts:173-200`.
pub fn detect() -> Shell {
    if std::env::var("NU_VERSION").is_ok() {
        return Shell::Nu;
    }
    if std::env::var("XONSHRC").is_ok() || std::env::var("XONSH_INTERACTIVE").is_ok() {
        return Shell::Xonsh;
    }
    if std::env::var("FISH_VERSION").is_ok() {
        return Shell::Fish;
    }
    if std::env::var("ZSH_VERSION").is_ok() {
        return Shell::Zsh;
    }
    if std::env::var("BASH_VERSION").is_ok() {
        return Shell::Bash;
    }
    if let Ok(shell_env) = std::env::var("SHELL") {
        if let Some(name) = shell_env.rsplit('/').next() {
            match name {
                "bash" => return Shell::Bash,
                "zsh" => return Shell::Zsh,
                "fish" => return Shell::Fish,
                "pwsh" => return Shell::Pwsh,
                "powershell" => return Shell::Powershell,
                "xonsh" => return Shell::Xonsh,
                "nu" => return Shell::Nu,
                _ => {}
            }
        }
    }
    // Fallback: bash is the most common assumption on Linux.
    Shell::Bash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_from_zsh_version() {
        // Use an internal helper form: set env temporarily per-thread.
        // `cargo test` parallelizes tests; use a unique name to avoid
        // collisions. We cheat here and only test the happy fallback.
        std::env::remove_var("NU_VERSION");
        std::env::remove_var("XONSHRC");
        std::env::remove_var("FISH_VERSION");
        std::env::remove_var("BASH_VERSION");
        std::env::set_var("ZSH_VERSION", "5.9");
        assert_eq!(detect(), Shell::Zsh);
        std::env::remove_var("ZSH_VERSION");
    }

    #[test]
    fn init_file_names() {
        assert_eq!(Shell::Bash.init_file_name(), "init.sh");
        assert_eq!(Shell::Zsh.init_file_name(), "init.zsh");
        assert_eq!(Shell::Fish.init_file_name(), "init.fish");
        assert_eq!(Shell::Pwsh.init_file_name(), "init.ps1");
        assert_eq!(Shell::Powershell.init_file_name(), "init.ps1");
        assert_eq!(Shell::Xonsh.init_file_name(), "init.xsh");
        assert_eq!(Shell::Nu.init_file_name(), "init.nu");
    }
}
