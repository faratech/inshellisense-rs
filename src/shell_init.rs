//! `insh init bash` and `insh install` — print or inject the bashrc snippet
//! that launches the user into an insh-wrapped bash when they open a new
//! shell. Idempotent; writes a marker line to avoid duplicate installs.

use anyhow::Result;

const MARKER: &str = "# >>> insh-rs init >>>";
const MARKER_END: &str = "# <<< insh-rs init <<<";

pub fn snippet() -> String {
    // Dual-guard both INSH_RS and ISTERM so this coexists with upstream
    // inshellisense's shell integration. Also respect VSCODE_RESOLVING_-
    // ENVIRONMENT per upstream to avoid interfering with VS Code env probes.
    format!(
        r#"{MARKER}
if [[ $- == *i* ]] && [[ -z "${{INSH_RS:-}}" ]] && [[ -z "${{ISTERM:-}}" ]] \
   && [[ -z "${{VSCODE_RESOLVING_ENVIRONMENT:-}}" ]] \
   && command -v insh >/dev/null 2>&1; then
    exec insh start
fi
{MARKER_END}
"#
    )
}

pub fn print_init(shell: &str) -> Result<()> {
    match shell {
        "bash" => {
            print!("{}", snippet());
            Ok(())
        }
        other => {
            anyhow::bail!("shell {other} not supported yet (bash only in this build)")
        }
    }
}

pub fn install() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    let rc = home.join(".bashrc");
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    if existing.contains(MARKER) {
        println!("insh-rs: already installed in {}", rc.display());
        return Ok(());
    }
    let mut new = existing;
    if !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&snippet());
    std::fs::write(&rc, new)?;
    println!("insh-rs: installed into {}", rc.display());
    println!("Open a new terminal or run `exec bash` to try it.");
    Ok(())
}
