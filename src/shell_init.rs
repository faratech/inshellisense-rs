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
use std::path::Path;

const MARKER: &str = "# >>> inshellisense-rs init >>>";
const MARKER_END: &str = "# <<< inshellisense-rs init <<<";

/// rc files are edited as raw bytes, never as `String`. A shell profile is
/// only required to be *shell*-readable, not valid UTF-8 — it may contain
/// latin-1 aliases, mid-file binary, or any other locale's bytes. Decoding
/// one lossily and writing the result back would silently corrupt it, and
/// `read_to_string(..).unwrap_or_default()` would truncate it outright.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}

/// Read an rc file as raw bytes. A missing file reads as empty; every other
/// error (permission denied, a directory, I/O failure) is reported rather
/// than being coerced into "empty", which would make the caller overwrite a
/// file it could not read.
fn read_rc_bytes(rc: &Path) -> Result<Vec<u8>> {
    match std::fs::read(rc) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", rc.display())),
    }
}

/// Trim ASCII whitespace (including a trailing `\r`) from both ends, so a
/// CRLF profile matches the same snippets an LF profile does.
fn trim_ascii(mut line: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = line {
        if first.is_ascii_whitespace() {
            line = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = line {
        if last.is_ascii_whitespace() {
            line = rest;
        } else {
            break;
        }
    }
    line
}

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

/// Append `snippet` to `rc` unless `marker` is already present. Existing
/// bytes are preserved verbatim — the snippet is only ever appended, so a
/// profile we cannot decode is still a profile we can safely extend.
fn append_snippet(rc: &Path, marker: &[u8], snippet: &str) -> Result<bool> {
    let existing = read_rc_bytes(rc)?;
    if contains_bytes(&existing, marker) {
        return Ok(false);
    }
    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut new = existing;
    if !new.ends_with(b"\n") {
        new.push(b'\n');
    }
    new.extend_from_slice(snippet.as_bytes());
    std::fs::write(rc, new).with_context(|| format!("writing {}", rc.display()))?;
    Ok(true)
}

pub fn install() -> Result<()> {
    let home = crate::paths::home().ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    let rc = home.join(".bashrc");
    if !append_snippet(&rc, MARKER.as_bytes(), &wrapper_snippet())? {
        println!("is: already installed in {}", rc.display());
        return Ok(());
    }
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
    let marker = snippet.trim().to_string();
    if !append_snippet(&rc, marker.as_bytes(), &snippet)? {
        println!(
            "is: {} init already installed in {}",
            shell.as_str(),
            rc.display()
        );
        return Ok(());
    }
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

/// Is our source snippet present as a real, executable line?
///
/// A substring search would accept `# [ -f ~/.inshellisense/... ] && source ...`
/// — a line the user deliberately commented out — as a working installation.
pub fn has_source_line(shell: Shell, contents: &[u8]) -> bool {
    let marker = source_marker(shell);
    contents
        .split_inclusive(|b| *b == b'\n')
        .any(|line| trim_ascii(line) == marker.as_bytes())
}

/// Is the `is install` auto-exec wrapper present, opened *and* closed?
pub fn has_wrapper_block(contents: &[u8]) -> bool {
    contents_has_block(contents).is_some()
}

fn contents_has_block(contents: &[u8]) -> Option<usize> {
    let start = find_bytes(contents, MARKER.as_bytes())?;
    let end_rel = find_bytes(&contents[start..], MARKER_END.as_bytes())?;
    Some(start + end_rel + MARKER_END.len())
}

/// Remove everything this tool installs, so a caller can inspect what is
/// left. Used by doctor to spot legacy upstream entries that coexist with a
/// current install — searching the raw text finds our own snippet, since it
/// also mentions `~/.inshellisense`.
pub fn strip_our_entries(shell: Shell, contents: &[u8]) -> Vec<u8> {
    let stripped = remove_marker_blocks(contents);
    remove_exact_snippet(&stripped, source_marker(shell).as_bytes())
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
    rc: &Path,
    snippets: &[String],
    remove_marker_block: bool,
    touched: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    let original = read_rc_bytes(rc)?;
    if original.is_empty() && !rc.exists() {
        return Ok(());
    }
    let mut text = original.clone();
    if remove_marker_block {
        text = remove_marker_blocks(&text);
    }
    for snippet in snippets {
        text = remove_exact_snippet(&text, snippet.as_bytes());
    }
    if text != original {
        std::fs::write(rc, &text).with_context(|| format!("writing {}", rc.display()))?;
        touched.push(rc.to_path_buf());
    }
    Ok(())
}

fn remove_marker_blocks(text: &[u8]) -> Vec<u8> {
    let mut text = text.to_vec();
    while let Some(start) = find_bytes(&text, MARKER.as_bytes()) {
        let Some(end_rel) = find_bytes(&text[start..], MARKER_END.as_bytes()) else {
            break;
        };
        let mut end = start + end_rel + MARKER_END.len();
        // Consume the block's own line terminator, CRLF or LF.
        if text.get(end) == Some(&b'\r') {
            end += 1;
        }
        if text.get(end) == Some(&b'\n') {
            end += 1;
        }
        text.drain(start..end);
    }
    text
}

/// Drop whole lines equal to `snippet`, leaving every other line — and its
/// original terminator — byte-for-byte intact. Rebuilding the file from
/// `str::lines()` would silently rewrite a CRLF profile to LF.
fn remove_exact_snippet(text: &[u8], snippet: &[u8]) -> Vec<u8> {
    let mut changed = false;
    let mut out = Vec::with_capacity(text.len());
    for line in text.split_inclusive(|b| *b == b'\n') {
        if trim_ascii(line) == snippet {
            changed = true;
        } else {
            out.extend_from_slice(line);
        }
    }
    if changed { out } else { text.to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASH_SNIPPET: &str =
        "[ -f ~/.inshellisense/init/bash/init.sh ] && source ~/.inshellisense/init/bash/init.sh";

    #[test]
    fn remove_exact_snippet_keeps_neighboring_lines_separate() {
        let input = format!("alias ll='ls -la'\n\n{BASH_SNIPPET}\nexport FOO=bar\n");
        let output = remove_exact_snippet(input.as_bytes(), BASH_SNIPPET.as_bytes());
        assert_eq!(output, b"alias ll='ls -la'\n\nexport FOO=bar\n");
    }

    /// A profile that is not valid UTF-8 must survive an install untouched
    /// apart from the appended snippet.
    #[test]
    fn append_snippet_preserves_non_utf8_bytes() {
        let dir = tempdir("append-non-utf8");
        let rc = dir.join(".bashrc");
        // `\xff` is not valid UTF-8 anywhere in a sequence.
        let original: &[u8] = b"alias caf\xe9='echo latin1'\n";
        std::fs::write(&rc, original).unwrap();

        assert!(append_snippet(&rc, MARKER.as_bytes(), &wrapper_snippet()).unwrap());

        let after = std::fs::read(&rc).unwrap();
        assert!(
            after.starts_with(original),
            "original bytes were not preserved: {after:?}"
        );
        assert!(contains_bytes(&after, MARKER.as_bytes()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Re-running install on a non-UTF-8 profile must be idempotent, not
    /// append a second copy.
    #[test]
    fn append_snippet_is_idempotent_on_non_utf8() {
        let dir = tempdir("append-idempotent");
        let rc = dir.join(".bashrc");
        std::fs::write(&rc, b"\xc3\x28 broken utf8\n").unwrap();
        assert!(append_snippet(&rc, MARKER.as_bytes(), &wrapper_snippet()).unwrap());
        let once = std::fs::read(&rc).unwrap();
        assert!(!append_snippet(&rc, MARKER.as_bytes(), &wrapper_snippet()).unwrap());
        assert_eq!(std::fs::read(&rc).unwrap(), once);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Uninstall must edit a non-UTF-8 profile rather than silently skipping it.
    #[test]
    fn remove_from_rc_edits_non_utf8_profile() {
        let dir = tempdir("remove-non-utf8");
        let rc = dir.join(".bashrc");
        let mut original = b"alias caf\xe9='echo latin1'\n".to_vec();
        original.extend_from_slice(BASH_SNIPPET.as_bytes());
        original.push(b'\n');
        std::fs::write(&rc, &original).unwrap();

        let mut touched = Vec::new();
        remove_from_rc(&rc, &[BASH_SNIPPET.to_string()], false, &mut touched).unwrap();

        assert_eq!(touched, vec![rc.clone()]);
        assert_eq!(
            std::fs::read(&rc).unwrap(),
            b"alias caf\xe9='echo latin1'\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Editing a CRLF profile must not rewrite the surviving lines to LF.
    #[test]
    fn remove_exact_snippet_preserves_crlf_line_endings() {
        let input = format!("alias ll='ls -la'\r\n{BASH_SNIPPET}\r\nexport FOO=bar\r\n");
        let output = remove_exact_snippet(input.as_bytes(), BASH_SNIPPET.as_bytes());
        assert_eq!(output, b"alias ll='ls -la'\r\nexport FOO=bar\r\n");
    }

    #[test]
    fn remove_marker_blocks_handles_crlf() {
        let input = format!("keep\r\n{MARKER}\r\nexec is start\r\n{MARKER_END}\r\ntail\r\n");
        let output = remove_marker_blocks(input.as_bytes());
        assert_eq!(output, b"keep\r\ntail\r\n");
    }

    /// A profile we cannot read must abort the write, not be replaced by the
    /// snippet alone.
    #[test]
    fn read_rc_bytes_reports_unreadable_paths() {
        let dir = tempdir("unreadable");
        // A directory stands in for any non-regular / unreadable path.
        assert!(read_rc_bytes(&dir).is_err());
        assert_eq!(
            read_rc_bytes(&dir.join("absent")).unwrap(),
            Vec::<u8>::new()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("insh-rs-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
