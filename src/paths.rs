//! Resource path helpers — centralized so the path layout can be audited
//! in one place.
//!
//! The layout mirrors upstream inshellisense's `~/.inshellisense/` tree,
//! just renamed to `~/.insh-rs/`. User config stays under the XDG-standard
//! `~/.config/insh-rs/` per our own convention.
//!
//! ```text
//! ~/.insh-rs/
//!   version.txt              # package version that unpacked this tree
//!   log/                     # verbose-mode debug logs
//!   shell/                   # canonical shell integration scripts
//!     shellIntegration.bash
//!     bash-preexec.sh
//!     shellIntegration-env.zsh
//!     shellIntegration-login.zsh
//!     shellIntegration-profile.zsh
//!     shellIntegration-rc.zsh
//!     shellIntegration.fish
//!     shellIntegration.ps1
//!     shellIntegration.xsh
//!     shellIntegration.nu
//!   init/                    # per-shell generated init files
//!     bash/init.sh
//!     zsh/init.zsh
//!     fish/init.fish
//!     pwsh/init.ps1
//!     powershell/init.ps1
//!     xonsh/init.xsh
//!     nu/init.nu
//!   zsh-dotdir/              # ZDOTDIR isolation target for zsh
//!     .zshenv
//!     .zshrc
//!     .zlogin
//!     .zprofile
//!   spec/                    # extras specs (loaded via INSH_RS_SPECS_DIR)
//! ```

use crate::shell::Shell;
use std::path::PathBuf;

pub fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// `~/.insh-rs/` — the resource root.
pub fn resource_root() -> Option<PathBuf> {
    home().map(|h| h.join(".insh-rs"))
}

pub fn version_file() -> Option<PathBuf> {
    resource_root().map(|r| r.join("version.txt"))
}

pub fn log_dir() -> Option<PathBuf> {
    resource_root().map(|r| r.join("log"))
}

/// `~/.insh-rs/shell/` — vendored shell integration scripts.
pub fn shell_dir() -> Option<PathBuf> {
    resource_root().map(|r| r.join("shell"))
}

/// `~/.insh-rs/init/<shell>/` — the per-shell generated init file directory.
pub fn init_dir(shell: Shell) -> Option<PathBuf> {
    resource_root().map(|r| r.join("init").join(shell.as_str()))
}

/// `~/.insh-rs/init/<shell>/init.<ext>` — the actual generated init file.
pub fn init_file(shell: Shell) -> Option<PathBuf> {
    init_dir(shell).map(|d| d.join(shell.init_file_name()))
}

/// `~/.insh-rs/zsh-dotdir/` — the isolated ZDOTDIR for zsh wrapping.
pub fn zsh_dotdir() -> Option<PathBuf> {
    resource_root().map(|r| r.join("zsh-dotdir"))
}

/// `~/.insh-rs/spec/` — runtime extras spec loader target.
pub fn spec_dir() -> Option<PathBuf> {
    resource_root().map(|r| r.join("spec"))
}

/// User-config root. XDG-standard: `~/.config/insh-rs/`.
pub fn user_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("insh-rs"))
}

/// `~/.config/insh-rs/rc.toml` — the primary config file path.
pub fn user_config_file() -> Option<PathBuf> {
    user_config_dir().map(|d| d.join("rc.toml"))
}

/// Upstream-compat config file paths. Read-only (we don't write to these).
pub fn upstream_config_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home() {
        out.push(home.join(".inshellisenserc"));
    }
    if let Some(cfg) = dirs::config_dir() {
        out.push(cfg.join("inshellisense").join("rc.toml"));
    }
    out
}

/// Resolve a shell's rc file (absolute path). Returns None for shells
/// whose rc file needs runtime resolution (e.g. pwsh's `$PROFILE`).
pub fn shell_rc_file(shell: Shell) -> Option<PathBuf> {
    let home = home()?;
    let rel = shell.rc_file_relative()?;
    Some(home.join(rel))
}
