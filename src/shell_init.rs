//! `is init <shell>` and `is install`.
//!
//! `print_init(<shell>)` emits the one-line source snippet the user
//! should add to their shell's rc file — byte-identical to upstream's
//! `is init <shell>` so our output can be dropped into an existing
//! `~/.bashrc` / `~/.zshrc` / etc. that previously had the upstream
//! line.
//!
//! `install()` is our own legacy convenience that appends an auto-exec
//! wrapper to `~/.bashrc` (`exec is start` on every interactive bash);
//! kept for backwards compatibility with the phase-0 flow, but not the
//! default path.

use crate::shell::Shell;
use anyhow::{Context, Result};

const MARKER: &str = "# >>> inshellisense-rs init >>>";
const MARKER_END: &str = "# <<< inshellisense-rs init <<<";

/// The auto-exec wrapper appended to `~/.bashrc` by `is install`.
/// Not used by `is init bash` — that path emits upstream's source
/// snippet instead.
pub fn wrapper_snippet() -> String {
    format!(
        r#"{MARKER}
if [[ $- == *i* ]] && [[ -z "${{INSH_RS:-}}" ]] && [[ -z "${{ISTERM:-}}" ]] \
   && [[ -z "${{VSCODE_RESOLVING_ENVIRONMENT:-}}" ]] \
   && command -v is >/dev/null 2>&1; then
    exec is start
fi
{MARKER_END}
"#
    )
}

/// Upstream-parity init snippet for each shell. The path template
/// matches `~/.inshellisense/init/<shell>/init.<ext>` shape — we use
/// `~/.inshellisense/` but otherwise emit byte-identical lines so migrating
/// users can swap out the path and everything else stays the same.
pub fn source_snippet(shell: Shell) -> String {
    let init_rel = match shell {
        Shell::Bash => "~/.inshellisense/init/bash/init.sh",
        Shell::Zsh => "~/.inshellisense/init/zsh/init.zsh",
        Shell::Fish => "~/.inshellisense/init/fish/init.fish",
        Shell::Pwsh => "~/.inshellisense/init/pwsh/init.ps1",
        Shell::Powershell => "~/.inshellisense/init/pwsh/init.ps1",
        Shell::Xonsh => "~/.inshellisense/init/xonsh/init.xsh",
        Shell::Nu => "~/.inshellisense/init/nu/init.nu",
        #[cfg(windows)]
        Shell::Cmd => return String::new(), // cmd.exe uses PROMPT env var, no init snippet
    };
    match shell {
        Shell::Bash => format!("\n\n[ -f {0} ] && source {0}\n", init_rel),
        Shell::Zsh => format!("\n\n[[ -f {0} ]] && source {0}\n", init_rel),
        Shell::Fish => format!("\n\ntest -f {0} && source {0}\n", init_rel),
        Shell::Pwsh | Shell::Powershell => format!(
            "\n\nif ( Test-Path '{0}' -PathType Leaf ) {{ . {0} }}\n",
            init_rel
        ),
        Shell::Xonsh => format!("\n\np\"{0}\".exists() && source \"{0}\"\n", init_rel),
        Shell::Nu => format!(
            "\n\nif ( '{0}' | path exists ) {{ source {0} }}\n",
            init_rel
        ),
        #[cfg(windows)]
        Shell::Cmd => unreachable!(), // handled above by early return
    }
}

pub fn print_init(shell: &str) -> Result<()> {
    let parsed = match shell {
        "bash" => Shell::Bash,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        "pwsh" => Shell::Pwsh,
        "powershell" => Shell::Powershell,
        "xonsh" => Shell::Xonsh,
        "nu" => Shell::Nu,
        other => anyhow::bail!("shell {other} not supported"),
    };
    print!("{}", source_snippet(parsed));
    Ok(())
}

pub fn install() -> Result<()> {
    let home = crate::paths::home().ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    let rc = home.join(".bashrc");
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(MARKER) {
        println!("is: already installed in {}", rc.display());
        return Ok(());
    }
    let mut new = existing;
    if !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&wrapper_snippet());
    std::fs::write(&rc, new)?;
    println!("is: installed into {}", rc.display());
    println!("Open a new terminal or run `exec bash` to try it.");
    Ok(())
}

pub fn install_rc(shell: Shell) -> Result<()> {
    let rc = crate::paths::shell_rc_file(shell).ok_or_else(|| {
        anyhow::anyhow!(
            "{} profile path cannot be resolved automatically; run `is init {}` and add the printed snippet manually",
            shell.as_str(),
            shell.as_str()
        )
    })?;
    let snippet = source_snippet(shell);
    let marker = snippet.trim();
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(marker) {
        println!(
            "is: {} init already installed in {}",
            shell.as_str(),
            rc.display()
        );
        return Ok(());
    }
    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut new = existing;
    if !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&snippet);
    std::fs::write(&rc, new).with_context(|| format!("writing {}", rc.display()))?;
    println!(
        "is: installed {} init into {}",
        shell.as_str(),
        rc.display()
    );
    Ok(())
}

pub fn source_marker(shell: Shell) -> String {
    source_snippet(shell).trim().to_string()
}

pub fn wrapper_marker() -> &'static str {
    MARKER
}

pub fn wrapper_end_marker() -> &'static str {
    MARKER_END
}

pub fn remove_installed_entries() -> Result<Vec<std::path::PathBuf>> {
    let mut touched = Vec::new();
    if let Some(rc) = crate::paths::home().map(|h| h.join(".bashrc")) {
        remove_from_rc(
            &rc,
            &[wrapper_snippet().trim().to_string()],
            true,
            &mut touched,
        )?;
    }
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Xonsh] {
        if let Some(rc) = crate::paths::shell_rc_file(shell) {
            remove_from_rc(&rc, &[source_marker(shell)], false, &mut touched)?;
        }
    }
    touched.sort();
    touched.dedup();
    Ok(touched)
}

fn remove_from_rc(
    rc: &std::path::Path,
    snippets: &[String],
    remove_marker_block: bool,
    touched: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    let Ok(mut text) = std::fs::read_to_string(rc) else {
        return Ok(());
    };
    let original = text.clone();
    if remove_marker_block {
        while let Some(start) = text.find(MARKER) {
            let Some(end_rel) = text[start..].find(MARKER_END) else {
                break;
            };
            let mut end = start + end_rel + MARKER_END.len();
            if text[end..].starts_with('\n') {
                end += 1;
            }
            text.replace_range(start..end, "");
        }
    }
    for snippet in snippets {
        text = remove_exact_snippet(&text, snippet);
    }
    if text != original {
        std::fs::write(rc, text).with_context(|| format!("writing {}", rc.display()))?;
        touched.push(rc.to_path_buf());
    }
    Ok(())
}

fn remove_exact_snippet(text: &str, snippet: &str) -> String {
    let mut changed = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim() == snippet {
            changed = true;
        } else {
            out.push(line);
        }
    }
    if !changed {
        return text.to_string();
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') && !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_exact_snippet_keeps_neighboring_lines_separate() {
        let snippet = "[ -f ~/.inshellisense/init/bash/init.sh ] && source ~/.inshellisense/init/bash/init.sh";
        let input = format!("alias ll='ls -la'\n\n{snippet}\nexport FOO=bar\n");
        let output = remove_exact_snippet(&input, snippet);
        assert_eq!(output, "alias ll='ls -la'\n\nexport FOO=bar\n");
    }
}
