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
use anyhow::Result;

const MARKER: &str = "# >>> insh-rs init >>>";
const MARKER_END: &str = "# <<< insh-rs init <<<";

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
/// `~/.insh-rs/` but otherwise emit byte-identical lines so migrating
/// users can swap out the path and everything else stays the same.
pub fn source_snippet(shell: Shell) -> String {
    let init_rel = match shell {
        Shell::Bash => "~/.insh-rs/init/bash/init.sh",
        Shell::Zsh => "~/.insh-rs/init/zsh/init.zsh",
        Shell::Fish => "~/.insh-rs/init/fish/init.fish",
        Shell::Pwsh => "~/.insh-rs/init/pwsh/init.ps1",
        Shell::Powershell => "~/.insh-rs/init/pwsh/init.ps1",
        Shell::Xonsh => "~/.insh-rs/init/xonsh/init.xsh",
        Shell::Nu => "~/.insh-rs/init/nu/init.nu",
    };
    match shell {
        Shell::Bash => format!(
            "\n\n[ -f {0} ] && source {0}\n",
            init_rel
        ),
        Shell::Zsh => format!(
            "\n\n[[ -f {0} ]] && source {0}\n",
            init_rel
        ),
        Shell::Fish => format!(
            "\n\ntest -f {0} && source {0}\n",
            init_rel
        ),
        Shell::Pwsh | Shell::Powershell => format!(
            "\n\nif ( Test-Path '{0}' -PathType Leaf ) {{ . {0} }}\n",
            init_rel
        ),
        Shell::Xonsh => format!(
            "\n\np\"{0}\".exists() && source \"{0}\"\n",
            init_rel
        ),
        Shell::Nu => format!(
            "\n\nif ( '{0}' | path exists ) {{ source {0} }}\n",
            init_rel
        ),
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
