//! Materialize vendored shell integration scripts into `~/.inshellisense/shell/`
//! and generate per-shell init files into `~/.inshellisense/init/<shell>/`.
//!
//! This is the equivalent of upstream's `unpackResources` + `createShellConfigs`
//! flow (`/tmp/inshellisense/src/utils/shell.ts:106-134`). Called eagerly
//! from `Cmd::Start`, `Cmd::Init`, and `Cmd::Reinit` so the runtime layout
//! is always in sync with the installed binary.
//!
//! The vendored scripts are compiled into the binary via `include_str!`.
//! No network access required.

use crate::paths;
use crate::shell::Shell;
use anyhow::{Context, Result};
use std::fs;

/// Every shell integration script we vendor, keyed by filename.
/// The extractor's output filename is what gets written into
/// `~/.inshellisense/shell/`.
const SHELL_SCRIPTS: &[(&str, &str)] = &[
    ("shellIntegration.bash", include_str!("../shell/shellIntegration.bash")),
    ("bash-preexec.sh", include_str!("../shell/bash-preexec.sh")),
    ("shellIntegration-env.zsh", include_str!("../shell/shellIntegration-env.zsh")),
    ("shellIntegration-login.zsh", include_str!("../shell/shellIntegration-login.zsh")),
    ("shellIntegration-profile.zsh", include_str!("../shell/shellIntegration-profile.zsh")),
    ("shellIntegration-rc.zsh", include_str!("../shell/shellIntegration-rc.zsh")),
    ("shellIntegration.fish", include_str!("../shell/shellIntegration.fish")),
    ("shellIntegration.ps1", include_str!("../shell/shellIntegration.ps1")),
    ("shellIntegration.xsh", include_str!("../shell/shellIntegration.xsh")),
    ("shellIntegration.nu", include_str!("../shell/shellIntegration.nu")),
];

/// All shells we ship init files for.
pub const ALL_SHELLS: &[Shell] = &[
    Shell::Bash,
    Shell::Zsh,
    Shell::Fish,
    Shell::Pwsh,
    Shell::Powershell,
    Shell::Xonsh,
    Shell::Nu,
];

/// Ensure `~/.inshellisense/` exists and contains the current version's
/// shell integration scripts + init files for every supported shell.
///
/// Returns the resource root path for logging/verification.
pub fn unpack() -> Result<std::path::PathBuf> {
    let root = paths::resource_root().context("no HOME directory")?;
    let shell_dir = paths::shell_dir().context("no HOME directory")?;

    // Skip re-unpacking if the version file already matches — avoids
    // disk churn on every `insh start`.
    if let Some(version_file) = paths::version_file() {
        if let Ok(existing) = fs::read_to_string(&version_file) {
            if existing.trim() == env!("CARGO_PKG_VERSION") {
                return Ok(root);
            }
        }
    }

    fs::create_dir_all(&root)
        .with_context(|| format!("creating {}", root.display()))?;
    fs::create_dir_all(&shell_dir)
        .with_context(|| format!("creating {}", shell_dir.display()))?;

    for (name, contents) in SHELL_SCRIPTS {
        let path = shell_dir.join(name);
        fs::write(&path, contents)
            .with_context(|| format!("writing {}", path.display()))?;
    }

    // Generate per-shell init files.
    for &shell in ALL_SHELLS {
        write_init_file(shell)?;
    }

    // Also populate the ZDOTDIR isolation directory for zsh.
    populate_zsh_dotdir()?;

    // Mark the tree as unpacked under this version.
    if let Some(version_file) = paths::version_file() {
        fs::write(&version_file, env!("CARGO_PKG_VERSION"))?;
    }

    Ok(root)
}

fn write_init_file(shell: Shell) -> Result<()> {
    let dir = paths::init_dir(shell).context("no HOME directory")?;
    let file = paths::init_file(shell).context("no HOME directory")?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let contents = init_file_contents(shell);
    fs::write(&file, contents)
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// The contents of `~/.inshellisense/init/<shell>/init.<ext>` — a one-line
/// source directive that pulls in the vendored shell integration.
fn init_file_contents(shell: Shell) -> String {
    let shell_dir = paths::shell_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "$HOME/.inshellisense/shell".to_string());
    match shell {
        Shell::Bash => format!(
            "# inshellisense-rs bash init — sourced from ~/.bashrc\n\
             if [ -f '{sd}/shellIntegration.bash' ]; then\n\
                 source '{sd}/shellIntegration.bash'\n\
             fi\n",
            sd = shell_dir
        ),
        Shell::Zsh => format!(
            "# inshellisense-rs zsh init — sourced from ~/.zshrc\n\
             if [[ -f '{sd}/shellIntegration-rc.zsh' ]]; then\n\
                 source '{sd}/shellIntegration-rc.zsh'\n\
             fi\n",
            sd = shell_dir
        ),
        Shell::Fish => format!(
            "# inshellisense-rs fish init — sourced from ~/.config/fish/config.fish\n\
             if test -f '{sd}/shellIntegration.fish'\n\
                 source '{sd}/shellIntegration.fish'\n\
             end\n",
            sd = shell_dir
        ),
        Shell::Pwsh | Shell::Powershell => format!(
            "# inshellisense-rs powershell init — sourced from $PROFILE\n\
             if ( Test-Path '{sd}/shellIntegration.ps1' -PathType Leaf ) {{\n\
                 . '{sd}/shellIntegration.ps1'\n\
             }}\n",
            sd = shell_dir
        ),
        Shell::Xonsh => format!(
            "# inshellisense-rs xonsh init\n\
             p'{sd}/shellIntegration.xsh'.exists() and source '{sd}/shellIntegration.xsh'\n",
            sd = shell_dir
        ),
        Shell::Nu => format!(
            "# inshellisense-rs nu init\n\
             if ('{sd}/shellIntegration.nu' | path exists) {{ source '{sd}/shellIntegration.nu' }}\n",
            sd = shell_dir
        ),
        #[cfg(windows)]
        Shell::Cmd => {
            // cmd.exe uses the PROMPT env var set at spawn time, no init file needed.
            String::new()
        }
    }
}

/// Populate `~/.inshellisense/zsh-dotdir/` with the four zsh startup files that
/// zsh reads when ZDOTDIR is set. The rc wrapper sources the user's original
/// interactive rc before installing prompt hooks; env/login/profile delegate
/// user startup sourcing to their matching integration scripts so each user
/// file is sourced exactly once.
///
/// Port of the flow at `/tmp/inshellisense/src/utils/shell.ts:54-163`.
fn populate_zsh_dotdir() -> Result<()> {
    let dir = paths::zsh_dotdir().context("no HOME directory")?;
    fs::create_dir_all(&dir)?;
    let shell_dir = paths::shell_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "$HOME/.inshellisense/shell".to_string());

    for (name, contents) in zsh_dotdir_contents(&shell_dir) {
        fs::write(dir.join(name), contents)?;
    }
    Ok(())
}

fn zsh_dotdir_contents(shell_dir: &str) -> Vec<(&'static str, String)> {
    let env_contents = format!(
        "# inshellisense-rs zsh .zshenv\n\
         if [[ -f '{sd}/shellIntegration-env.zsh' ]]; then\n\
             source '{sd}/shellIntegration-env.zsh'\n\
         fi\n",
        sd = shell_dir
    );
    let rc_contents = format!(
        "# inshellisense-rs zsh .zshrc\n\
         if [[ -n \"${{USER_ZDOTDIR:-}}\" && -f \"${{USER_ZDOTDIR}}/.zshrc\" ]]; then\n\
             source \"${{USER_ZDOTDIR}}/.zshrc\"\n\
         fi\n\
         if [[ -f '{sd}/shellIntegration-rc.zsh' ]]; then\n\
             source '{sd}/shellIntegration-rc.zsh'\n\
         fi\n",
        sd = shell_dir
    );
    let login_contents = format!(
        "# inshellisense-rs zsh .zlogin\n\
         if [[ -f '{sd}/shellIntegration-login.zsh' ]]; then\n\
             source '{sd}/shellIntegration-login.zsh'\n\
         fi\n",
        sd = shell_dir
    );
    let profile_contents = format!(
        "# inshellisense-rs zsh .zprofile\n\
         if [[ -f '{sd}/shellIntegration-profile.zsh' ]]; then\n\
             source '{sd}/shellIntegration-profile.zsh'\n\
        fi\n",
        sd = shell_dir
    );

    vec![
        (".zshenv", env_contents),
        (".zshrc", rc_contents),
        (".zlogin", login_contents),
        (".zprofile", profile_contents),
    ]
}

/// Remove the entire `~/.inshellisense/` tree. Called by `insh uninstall`.
/// Returns Ok(()) if already absent.
pub fn remove_all() -> Result<()> {
    let Some(root) = paths::resource_root() else {
        return Ok(());
    };
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("removing {}", root.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated_zsh_file(name: &str) -> String {
        zsh_dotdir_contents("/tmp/is-shell")
            .into_iter()
            .find(|(file, _)| *file == name)
            .map(|(_, contents)| contents)
            .unwrap()
    }

    fn vendored_script(name: &str) -> &'static str {
        SHELL_SCRIPTS
            .iter()
            .find(|(file, _)| *file == name)
            .map(|(_, contents)| *contents)
            .unwrap()
    }

    #[test]
    fn generated_zsh_env_login_profile_do_not_double_source_user_files() {
        for file in [".zshenv", ".zlogin", ".zprofile"] {
            let contents = generated_zsh_file(file);
            assert!(
                !contents.contains("USER_ZDOTDIR"),
                "{file} should delegate user startup sourcing to the integration script"
            );
        }
        let rc = generated_zsh_file(".zshrc");
        assert!(rc.contains("USER_ZDOTDIR"));
        assert!(rc.contains(".zshrc"));
    }

    #[test]
    fn vendored_zsh_and_fish_scripts_have_expected_safe_content() {
        assert!(!vendored_script("shellIntegration-rc.zsh").contains(".zshrc"));
        assert!(vendored_script("shellIntegration-env.zsh").contains(".zshenv"));
        assert!(vendored_script("shellIntegration-login.zsh").contains(".zlogin"));
        assert!(vendored_script("shellIntegration-profile.zsh").contains(".zprofile"));
        assert!(vendored_script("shellIntegration.fish").contains("printf '%s'"));
    }
}
