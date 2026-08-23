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

/// Every file `unpack()` is responsible for materializing, paired with the
/// exact bytes this build would put in it.
fn expected_contents() -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    if let Some(shell_dir) = paths::shell_dir() {
        out.extend(
            SHELL_SCRIPTS
                .iter()
                .map(|(name, contents)| (shell_dir.join(name), (*contents).to_string())),
        );
    }
    for &shell in ALL_SHELLS {
        if let Some(file) = paths::init_file(shell) {
            out.push((file, init_file_contents(shell)));
        }
    }
    if let Some(dotdir) = paths::zsh_dotdir()
        && let Some(shell_dir) = paths::shell_dir()
    {
        // zsh_dotdir_contents interpolates the *shell* dir into the
        // scripts; the files themselves live in the ZDOTDIR directory.
        let shell_dir = shell_dir.display().to_string();
        out.extend(
            zsh_dotdir_contents(&shell_dir)
                .into_iter()
                .map(|(name, contents)| (dotdir.join(name), contents)),
        );
    }
    out
}

/// Every file `unpack()` is responsible for materializing.
pub fn expected_files() -> Vec<std::path::PathBuf> {
    expected_contents().into_iter().map(|(p, _)| p).collect()
}

/// Expected files that are not on disk. Empty means the tree is complete.
pub fn missing_files() -> Vec<std::path::PathBuf> {
    expected_files()
        .into_iter()
        .filter(|p| !p.exists())
        .collect()
}

/// Expected files whose on-disk bytes differ from what this build would
/// write. A truncated or hand-edited script used to be invisible: the
/// version stamp still matched, so `unpack()` skipped the repair forever and
/// every wrapped session sourced a broken script while `is doctor` called
/// the tree healthy.
pub fn mismatched_files() -> Vec<std::path::PathBuf> {
    expected_contents()
        .into_iter()
        .filter(|(path, contents)| {
            fs::read(path)
                .map(|bytes| bytes != contents.as_bytes())
                .unwrap_or(true)
        })
        .map(|(path, _)| path)
        .collect()
}

/// True when the on-disk tree matches this binary's content stamp, no
/// expected file has gone missing, and every expected file holds the exact
/// bytes this build writes.
pub fn tree_is_current() -> bool {
    let Some(version_file) = paths::version_file() else {
        return false;
    };
    let Ok(existing) = fs::read_to_string(&version_file) else {
        return false;
    };
    existing.trim() == fingerprint() && missing_files().is_empty() && mismatched_files().is_empty()
}

/// Write `contents` to `path` atomically: the final name only ever appears
/// once the whole file is on disk. A plain `fs::write` truncates the
/// destination first, so a crash or ENOSPC mid-write left a partial script
/// behind that the version stamp still vouched for.
fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
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
        write_atomic(&path, contents).with_context(|| format!("writing {}", path.display()))?;
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

    // Verify the pass actually produced this build's bytes. Reporting success
    // over a tree that still fails `tree_is_current()` would leave the user
    // with a broken install and no diagnostic.
    let broken = missing_files();
    let broken = if broken.is_empty() {
        mismatched_files()
    } else {
        broken
    };
    if !broken.is_empty() {
        let names: Vec<String> = broken.iter().map(|p| p.display().to_string()).collect();
        anyhow::bail!(
            "resource repair did not take effect for: {}",
            names.join(", ")
        );
    }

    Ok(root)
}

fn write_init_file(shell: Shell) -> Result<()> {
    let dir = paths::init_dir(shell).context("no HOME directory")?;
    let file = paths::init_file(shell).context("no HOME directory")?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let contents = init_file_contents(shell);
    write_atomic(&file, &contents).with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Escape a path for embedding inside a POSIX single-quoted string
/// (bash/zsh). `'` closes the quote, so it must be spliced out and back in.
fn quote_posix(path: &str) -> String {
    path.replace('\'', "'\\''")
}

/// fish honors `\` and `\'` inside single quotes.
pub(crate) fn quote_fish(path: &str) -> String {
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
        write_atomic(&dir.join(name), &contents)?;
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

    /// `set_var("HOME")` mutates process-global state, so HOME-scoped tests
    /// run under one lock rather than racing sibling tests.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `check` with HOME (USERPROFILE on Windows) pointed at a fresh
    /// temporary directory, restoring the previous value afterwards.
    fn with_temp_home(check: impl FnOnce(&std::path::Path)) {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "insh-rs-resources-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        let var = "HOME";
        #[cfg(windows)]
        let var = "USERPROFILE";
        let previous = std::env::var_os(var);
        // SAFETY: serialized by HOME_LOCK, restored before returning.
        unsafe { std::env::set_var(var, &dir) };
        check(&dir);
        // SAFETY: as above.
        unsafe {
            match previous {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A truncated integration script used to keep the matching version stamp
    /// forever: `unpack()` skipped the repair and `is doctor` called the tree
    /// healthy, so every wrapped session sourced an empty script.
    #[test]
    fn truncated_script_is_detected_and_repaired() {
        with_temp_home(|_home| {
            unpack().expect("initial unpack");
            assert!(tree_is_current(), "fresh tree must be current");

            let script = paths::shell_dir().unwrap().join("shellIntegration.bash");
            fs::write(&script, "").unwrap();
            assert!(
                !tree_is_current(),
                "a matching stamp must not vouch for truncated content"
            );
            assert_eq!(mismatched_files(), vec![script.clone()]);

            unpack().expect("repairing unpack");
            assert_eq!(
                fs::read(&script).unwrap(),
                include_bytes!("../shell/shellIntegration.bash").as_slice()
            );
            assert!(tree_is_current(), "repair must restore currency");
        });
    }

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

    /// Resource writes go through a temp file + rename so an interrupted pass
    /// can never leave a truncated script at the final path, and no temp
    /// file lingers afterwards.
    #[test]
    fn atomic_write_replaces_exactly_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("insh-rs-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.sh");
        fs::write(&path, "stale bytes from a previous install").unwrap();
        write_atomic(&path, "fresh\nbytes\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh\nbytes\n");
        let count = fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 1, "the temp file must be gone, not lingering");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Every expected file has known content to compare against — otherwise
    /// `mismatched_files()` would silently check nothing.
    #[test]
    fn expected_contents_covers_every_vendored_and_generated_file() {
        // 10 vendored scripts + 7 init files + 4 zsh dotdir files.
        assert_eq!(
            expected_contents().len(),
            SHELL_SCRIPTS.len() + ALL_SHELLS.len() + 4
        );
        for (_, contents) in expected_contents() {
            assert!(!contents.is_empty());
        }
    }
}
