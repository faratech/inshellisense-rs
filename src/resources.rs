//! Materialize vendored shell integration scripts into `~/.inshellisense/shell/`
//! and generate per-shell init files into `~/.inshellisense/init/<shell>/`.
//!
//! This is the equivalent of upstream's `unpackResources` + `createShellConfigs`
//! flow (upstream inshellisense's `src/utils/shell.ts:106-134`). Called eagerly
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
    (
        "shellIntegration.bash",
        include_str!("../shell/shellIntegration.bash"),
    ),
    ("bash-preexec.sh", include_str!("../shell/bash-preexec.sh")),
    (
        "shellIntegration-env.zsh",
        include_str!("../shell/shellIntegration-env.zsh"),
    ),
    (
        "shellIntegration-login.zsh",
        include_str!("../shell/shellIntegration-login.zsh"),
    ),
    (
        "shellIntegration-profile.zsh",
        include_str!("../shell/shellIntegration-profile.zsh"),
    ),
    (
        "shellIntegration-rc.zsh",
        include_str!("../shell/shellIntegration-rc.zsh"),
    ),
    (
        "shellIntegration.fish",
        include_str!("../shell/shellIntegration.fish"),
    ),
    (
        "shellIntegration.ps1",
        include_str!("../shell/shellIntegration.ps1"),
    ),
    (
        "shellIntegration.xsh",
        include_str!("../shell/shellIntegration.xsh"),
    ),
    (
        "shellIntegration.nu",
        include_str!("../shell/shellIntegration.nu"),
    ),
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

/// FNV-1a. Stable across toolchains and platforms, unlike `DefaultHasher`,
/// so an unrelated Rust upgrade doesn't invalidate every user's cache.
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Identity of the resource tree this binary would write: the version plus
/// the *content* of every file in it.
///
/// Gating on the version alone meant a same-version binary with changed
/// integration scripts — and any user who deleted a script by hand — kept the
/// stale tree forever, because `unpack()` returned early before checking that
/// the files it promises actually exist.
pub fn fingerprint() -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    hash = fnv1a(env!("CARGO_PKG_VERSION").as_bytes(), hash);
    for (name, contents) in SHELL_SCRIPTS {
        hash = fnv1a(name.as_bytes(), hash);
        hash = fnv1a(contents.as_bytes(), hash);
    }
    // Init files embed the absolute resource root, so they must contribute.
    for &shell in ALL_SHELLS {
        hash = fnv1a(init_file_contents(shell).as_bytes(), hash);
    }
    let shell_dir = paths::shell_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    for (name, contents) in zsh_dotdir_contents(&shell_dir) {
        hash = fnv1a(name.as_bytes(), hash);
        hash = fnv1a(contents.as_bytes(), hash);
    }
    format!("{} {:016x}", env!("CARGO_PKG_VERSION"), hash)
}

/// Every file `unpack()` is responsible for materializing.
pub fn expected_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(shell_dir) = paths::shell_dir() {
        out.extend(SHELL_SCRIPTS.iter().map(|(name, _)| shell_dir.join(name)));
    }
    for &shell in ALL_SHELLS {
        if let Some(file) = paths::init_file(shell) {
            out.push(file);
        }
    }
    if let Some(dotdir) = paths::zsh_dotdir() {
        for name in [".zshenv", ".zshrc", ".zlogin", ".zprofile"] {
            out.push(dotdir.join(name));
        }
    }
    out
}

/// Expected files that are not on disk. Empty means the tree is complete.
pub fn missing_files() -> Vec<std::path::PathBuf> {
    expected_files()
        .into_iter()
        .filter(|p| !p.exists())
        .collect()
}

/// True when the on-disk tree matches this binary's content stamp and no
/// expected file has gone missing.
pub fn tree_is_current() -> bool {
    let Some(version_file) = paths::version_file() else {
        return false;
    };
    let Ok(existing) = fs::read_to_string(&version_file) else {
        return false;
    };
    existing.trim() == fingerprint() && missing_files().is_empty()
}

/// Ensure `~/.inshellisense/` exists and contains the current version's
/// shell integration scripts + init files for every supported shell.
///
/// Returns the resource root path for logging/verification.
pub fn unpack() -> Result<std::path::PathBuf> {
    let root = paths::resource_root().context("no HOME directory")?;
    let shell_dir = paths::shell_dir().context("no HOME directory")?;

    // Skip re-unpacking only when the tree is byte-current AND complete.
    if tree_is_current() {
        return Ok(root);
    }

    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    fs::create_dir_all(&shell_dir).with_context(|| format!("creating {}", shell_dir.display()))?;

    for (name, contents) in SHELL_SCRIPTS {
        let path = shell_dir.join(name);
        fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }

    // Generate per-shell init files.
    for &shell in ALL_SHELLS {
        write_init_file(shell)?;
    }

    // Also populate the ZDOTDIR isolation directory for zsh.
    populate_zsh_dotdir()?;

    // Stamp the tree with the content fingerprint it was written from.
    if let Some(version_file) = paths::version_file() {
        fs::write(&version_file, fingerprint())?;
    }

    Ok(root)
}

fn write_init_file(shell: Shell) -> Result<()> {
    let dir = paths::init_dir(shell).context("no HOME directory")?;
    let file = paths::init_file(shell).context("no HOME directory")?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let contents = init_file_contents(shell);
    fs::write(&file, contents).with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Escape a path for embedding inside a POSIX single-quoted string
/// (bash/zsh). `'` closes the quote, so it must be spliced out and back in.
fn quote_posix(path: &str) -> String {
    path.replace('\'', "'\\''")
}

/// fish honors `\` and `\'` inside single quotes.
fn quote_fish(path: &str) -> String {
    path.replace('\\', "\\\\").replace('\'', "\\'")
}

/// PowerShell doubles an embedded single quote.
fn quote_pwsh(path: &str) -> String {
    path.replace('\'', "''")
}

/// xonsh strings are Python strings: escape backslashes (so a Windows path
/// never forms a `\U` escape) and single quotes.
fn quote_python(path: &str) -> String {
    path.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Nushell single-quoted strings have no escape syntax, so use a
/// double-quoted string, which does.
fn quote_nu(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The contents of `~/.inshellisense/init/<shell>/init.<ext>`.
///
/// Each file does two things, in this order:
///
///  1. **Inside** a wrapped session (`ISTERM`/`INSH_RS` set): source the OSC
///     6973 integration so the wrapper can see prompt markers.
///  2. **Outside** one, in an interactive shell: `exec is start`, replacing
///     the shell with the wrapped one.
///
/// Step 2 used to be missing entirely. The init file only installed the
/// marker hooks, so an ordinary shell emitted OSC 6973 sequences that no
/// process was listening for, and no suggestions ever appeared.
///
/// The resource root is derived from `$HOME`, which may legitimately contain
/// a single quote (`/home/o'brien`). Interpolating it raw would truncate the
/// quoted string and leave the rest of the path as shell code.
fn init_file_contents(shell: Shell) -> String {
    let shell_dir = paths::shell_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "$HOME/.inshellisense/shell".to_string());
    match shell {
        Shell::Bash => format!(
            "# inshellisense-rs bash init — sourced from ~/.bashrc\n\
             if [ -n \"${{ISTERM:-}}${{INSH_RS:-}}\" ]; then\n\
             \x20   [ -f '{sd}/shellIntegration.bash' ] && source '{sd}/shellIntegration.bash'\n\
             elif [[ $- == *i* ]] && [ -z \"${{VSCODE_RESOLVING_ENVIRONMENT:-}}\" ] \\\n\
             \x20    && command -v is >/dev/null 2>&1; then\n\
             \x20   exec is start\n\
             fi\n",
            sd = quote_posix(&shell_dir)
        ),
        Shell::Zsh => format!(
            "# inshellisense-rs zsh init — sourced from ~/.zshrc\n\
             if [[ -n \"${{ISTERM:-}}${{INSH_RS:-}}\" ]]; then\n\
             \x20   [[ -f '{sd}/shellIntegration-rc.zsh' ]] && source '{sd}/shellIntegration-rc.zsh'\n\
             elif [[ -o interactive ]] && [[ -z \"${{VSCODE_RESOLVING_ENVIRONMENT:-}}\" ]] \\\n\
             \x20    && (( $+commands[is] )); then\n\
             \x20   exec is start\n\
             fi\n",
            sd = quote_posix(&shell_dir)
        ),
        Shell::Fish => format!(
            "# inshellisense-rs fish init — sourced from ~/.config/fish/config.fish\n\
             if set -q ISTERM; or set -q INSH_RS\n\
             \x20   test -f '{sd}/shellIntegration.fish'; and source '{sd}/shellIntegration.fish'\n\
             else if status is-interactive; and not set -q VSCODE_RESOLVING_ENVIRONMENT; and command -q is\n\
             \x20   exec is start\n\
             end\n",
            sd = quote_fish(&shell_dir)
        ),
        // PowerShell continues a line with a backtick, never a backslash.
        Shell::Pwsh | Shell::Powershell => format!(
            "# inshellisense-rs powershell init — sourced from $PROFILE\n\
             if ( $env:ISTERM -or $env:INSH_RS ) {{\n\
             \x20   if ( Test-Path '{sd}/shellIntegration.ps1' -PathType Leaf ) {{ . '{sd}/shellIntegration.ps1' }}\n\
             }} elseif ( [Environment]::UserInteractive -and -not $env:VSCODE_RESOLVING_ENVIRONMENT -and (Get-Command is -ErrorAction SilentlyContinue) ) {{\n\
             \x20   is start\n\
             \x20   exit\n\
             }}\n",
            sd = quote_pwsh(&shell_dir)
        ),
        Shell::Xonsh => format!(
            "# inshellisense-rs xonsh init\n\
             import os, shutil\n\
             if os.environ.get('ISTERM') or os.environ.get('INSH_RS'):\n\
             \x20   p'{sd}/shellIntegration.xsh'.exists() and source '{sd}/shellIntegration.xsh'\n\
             elif not os.environ.get('VSCODE_RESOLVING_ENVIRONMENT') and shutil.which('is'):\n\
             \x20   os.execvp('is', ['is', 'start'])\n",
            sd = quote_python(&shell_dir)
        ),
        Shell::Nu => format!(
            "# inshellisense-rs nu init\n\
             if ('ISTERM' in $env) or ('INSH_RS' in $env) {{\n\
             \x20   if (\"{sd}/shellIntegration.nu\" | path exists) {{ source \"{sd}/shellIntegration.nu\" }}\n\
             }} else if ('VSCODE_RESOLVING_ENVIRONMENT' not-in $env) and ((which is | length) > 0) {{\n\
             \x20   exec is start\n\
             }}\n",
            sd = quote_nu(&shell_dir)
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
/// Port of the flow at upstream inshellisense's `src/utils/shell.ts:54-163`.
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
    let shell_dir = quote_posix(shell_dir);
    let shell_dir = shell_dir.as_str();
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

/// Remove the entire `~/.inshellisense/` tree. Called by `is uninstall`.
/// Returns Ok(()) if already absent.
pub fn remove_all() -> Result<()> {
    let Some(root) = paths::resource_root() else {
        return Ok(());
    };
    if root.exists() {
        fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod init_file_tests {
    use super::*;

    /// The generated init file must both load the integration inside a
    /// session and start the wrapper outside one. Installing only the OSC
    /// hooks meant an ordinary shell emitted markers nothing was listening
    /// for, so no suggestions ever appeared.
    #[test]
    fn init_files_start_the_wrapper_outside_a_session() {
        for &shell in ALL_SHELLS {
            let contents = init_file_contents(shell);
            assert!(
                contents.contains("ISTERM") && contents.contains("INSH_RS"),
                "{} init does not check for an existing session:\n{contents}",
                shell.as_str()
            );
            let starts_wrapper =
                contents.contains("is start") || contents.contains("'is', 'start'");
            assert!(
                starts_wrapper,
                "{} init never starts the wrapper:\n{contents}",
                shell.as_str()
            );
        }
    }

    /// `~/.bashrc` -> `init.sh` -> `shellIntegration.bash` -> `~/.bashrc`
    /// recursed until bash died. The init file must only source the
    /// integration when already inside a session.
    #[test]
    fn bash_init_only_sources_integration_inside_a_session() {
        let contents = init_file_contents(Shell::Bash);
        let source_line = contents
            .lines()
            .find(|l| l.contains("shellIntegration.bash"))
            .expect("integration source line");
        let guard_idx = contents.find("ISTERM").expect("session guard");
        let source_idx = contents.find(source_line).unwrap();
        assert!(
            guard_idx < source_idx,
            "the session guard must precede the source:\n{contents}"
        );
    }
}

#[cfg(test)]
mod quoting_tests {
    use super::*;

    /// `$HOME` may contain a single quote. Interpolating it raw closed the
    /// quoted string and left the remainder of the path as shell code.
    #[test]
    fn posix_quoting_survives_apostrophe() {
        assert_eq!(quote_posix("/home/o'brien/x"), "/home/o'\\''brien/x");
    }

    #[test]
    fn fish_and_python_quoting_escape_backslash_and_quote() {
        assert_eq!(quote_fish("/home/o'b"), "/home/o\\'b");
        assert_eq!(quote_python(r"C:\Users\o'b"), r"C:\\Users\\o\'b");
    }

    #[test]
    fn pwsh_quoting_doubles_apostrophe() {
        assert_eq!(quote_pwsh("/home/o'brien"), "/home/o''brien");
    }

    #[test]
    fn nu_quoting_escapes_double_quote() {
        assert_eq!(quote_nu(r#"/home/a"b"#), r#"/home/a\"b"#);
    }
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
