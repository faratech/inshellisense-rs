//! Shell abstraction — enum, detection, source commands, init snippets.
//!
//! Port of upstream inshellisense's `src/utils/shell.ts`. Handles the supported shells:
//!   bash, zsh, fish, pwsh (PowerShell Core), powershell (Windows),
//!   xonsh, nu (Nushell), and cmd.exe on Windows.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Pwsh,
    Powershell,
    Xonsh,
    Nu,
    #[cfg(windows)]
    Cmd,
}

/// Complete spawn descriptor for `pty::run_wrapped_shell`. Computed from
/// a `Shell` value via `Shell::spawn_target`. Port of
/// upstream inshellisense's `src/isterm/pty.ts:377-429` (`convertToPtyTarget`).
pub struct SpawnTarget {
    /// The shell binary name or absolute path (passed to portable-pty).
    pub binary: String,
    /// Arguments to pass to the shell.
    pub args: Vec<String>,
    /// Extra env vars to set on the child process (on top of the parent's env
    /// + the dual-guard ISTERM/INSH_RS markers).
    pub env: Vec<(String, String)>,
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
            #[cfg(windows)]
            Shell::Cmd => "cmd",
        }
    }

    /// Compute the binary, argv, and env overrides needed to spawn this
    /// shell under a PTY with our integration script sourced at startup.
    ///
    /// `shell_dir` is the path to `~/.inshellisense/shell/` (absolute). If the
    /// directory doesn't exist yet, callers should run
    /// `crate::resources::unpack()` first.
    pub fn spawn_target(
        self,
        shell_dir: &std::path::Path,
        zsh_dotdir: &std::path::Path,
        login: bool,
    ) -> SpawnTarget {
        let path_of = |name: &str| shell_dir.join(name).display().to_string();
        let mut args: Vec<String> = Vec::new();
        let mut env: Vec<(String, String)> = Vec::new();
        let binary = self.as_str().to_string();

        match self {
            Shell::Bash => {
                args.push("--init-file".into());
                args.push(path_of("shellIntegration.bash"));
                // Tells shellIntegration.bash that bash did *not* read the
                // user's startup files itself, so the script must source them.
                // Unset by the script, so nested shells don't re-source them.
                env.push(("INSH_RS_BASH_INIT_FILE".into(), "1".into()));
            }
            Shell::Zsh => {
                // Preserve the user's original ZDOTDIR (so their .zshrc etc.
                // can still be sourced from the wrapper scripts) and point
                // zsh at our isolated dotdir.
                if let Ok(user_zdotdir) = std::env::var("ZDOTDIR") {
                    env.push(("USER_ZDOTDIR".into(), user_zdotdir));
                } else if let Some(home) = std::env::var_os("HOME") {
                    env.push(("USER_ZDOTDIR".into(), home.to_string_lossy().into_owned()));
                }
                env.push(("ZDOTDIR".into(), zsh_dotdir.display().to_string()));
            }
            Shell::Fish => {
                args.push("--init-command".into());
                args.push(format!("source {}", path_of("shellIntegration.fish")));
            }
            Shell::Pwsh | Shell::Powershell => {
                args.push("-NoExit".into());
                args.push("-Command".into());
                args.push(format!(
                    "try {{ . \"{}\" }} catch {{}}",
                    path_of("shellIntegration.ps1")
                ));
            }
            Shell::Xonsh => {
                // Include existing user configs so our wrapper doesn't
                // shadow them — matches upstream's ordering.
                args.push("--rc".into());
                if let Some(home) = crate::paths::home() {
                    let candidates = [home.join(".xonshrc"), home.join(".config/xonsh/rc.xsh")];
                    for c in &candidates {
                        if c.exists() {
                            args.push(c.display().to_string());
                        }
                    }
                }
                args.push(path_of("shellIntegration.xsh"));
            }
            Shell::Nu => {
                args.push("--execute".into());
                args.push(format!("source `{}`", path_of("shellIntegration.nu")));
            }
            #[cfg(windows)]
            Shell::Cmd => {
                // cmd.exe uses the PROMPT env var for OSC 6973 markers.
                // No shell integration script needed. `$P` expands to the
                // current drive and path — without the CWD marker the tracker
                // never learned the directory, so filepath completion resolved
                // against an empty base.
                env.push((
                    "PROMPT".into(),
                    "\x1b]6973;PS\x07\x1b]6973;CWD;$P\x07$P$G \x1b]6973;PE\x07".into(),
                ));
            }
        }

        if login {
            match self {
                // Bash deliberately gets no `--login`. An interactive *login*
                // bash ignores `--init-file` outright and reads
                // `~/.bash_profile` instead, so passing both silently dropped
                // the shell integration. `shellIntegration.bash` performs the
                // login startup sequence itself when `ISTERM_LOGIN` is set.
                Shell::Bash => {}
                Shell::Zsh | Shell::Fish | Shell::Xonsh | Shell::Nu => {
                    args.insert(0, "--login".into())
                }
                Shell::Pwsh | Shell::Powershell => args.insert(0, "-Login".into()),
                #[cfg(windows)]
                Shell::Cmd => {} // cmd has no login concept
            }
        }

        SpawnTarget { binary, args, env }
    }

    /// Filename for the generated init file in ~/.inshellisense/init/<shell>/<file>.
    pub fn init_file_name(self) -> &'static str {
        match self {
            Shell::Bash => "init.sh",
            Shell::Zsh => "init.zsh",
            Shell::Fish => "init.fish",
            Shell::Pwsh | Shell::Powershell => "init.ps1",
            Shell::Xonsh => "init.xsh",
            Shell::Nu => "init.nu",
            #[cfg(windows)]
            Shell::Cmd => "init.cmd",
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
/// upstream inshellisense's `src/utils/shell.ts:173-200`.
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
        // Extract the binary name from the path. Handle both `/`
        // (Unix, MSYS2) and `\` (Windows) separators, and strip
        // a trailing `.exe` if present.
        let name = shell_env
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .strip_suffix(".exe")
            .unwrap_or(shell_env.rsplit(['/', '\\']).next().unwrap_or(""));
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
    // Fallback: platform-specific default.
    #[cfg(windows)]
    {
        // Prefer pwsh (PowerShell Core), then legacy powershell, then cmd.
        if crate::platform::find_on_path("pwsh").is_some() {
            return Shell::Pwsh;
        }
        if crate::platform::find_on_path("powershell").is_some() {
            return Shell::Powershell;
        }
        return Shell::Cmd;
    }
    #[cfg(not(windows))]
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
        unsafe {
            std::env::remove_var("NU_VERSION");
            std::env::remove_var("XONSHRC");
            std::env::remove_var("FISH_VERSION");
            std::env::remove_var("BASH_VERSION");
            std::env::set_var("ZSH_VERSION", "5.9");
        }
        assert_eq!(detect(), Shell::Zsh);
        unsafe {
            std::env::remove_var("ZSH_VERSION");
        }
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

    /// An interactive *login* bash ignores `--init-file` and reads
    /// `~/.bash_profile` instead, so passing both silently dropped the shell
    /// integration. `shellIntegration.bash` replays the login sequence itself
    /// when `ISTERM_LOGIN` is set, so bash must not be given `--login`.
    #[test]
    fn login_bash_keeps_init_file_and_drops_login_flag() {
        let shell_dir = std::path::Path::new("/tmp/shell");
        let zsh_dotdir = std::path::Path::new("/tmp/zdot");
        let target = Shell::Bash.spawn_target(shell_dir, zsh_dotdir, true);
        assert!(
            !target.args.iter().any(|a| a == "--login"),
            "bash must not be spawned with --login: {:?}",
            target.args
        );
        assert_eq!(target.args[0], "--init-file");
        assert!(target.args[1].ends_with("shellIntegration.bash"));
        // Tells the script that bash skipped the user's startup files.
        assert!(
            target
                .env
                .iter()
                .any(|(k, v)| k == "INSH_RS_BASH_INIT_FILE" && v == "1")
        );
    }

    /// Other shells still honor `--login`; only bash has the conflict.
    #[test]
    fn login_zsh_still_gets_login_flag() {
        let target = Shell::Zsh.spawn_target(
            std::path::Path::new("/tmp/shell"),
            std::path::Path::new("/tmp/zdot"),
            true,
        );
        assert_eq!(target.args.first().map(String::as_str), Some("--login"));
    }
}
