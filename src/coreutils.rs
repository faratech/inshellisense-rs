//! Microsoft Coreutils for Windows / uutils coreutils integration.
//!
//! [Coreutils for Windows](https://github.com/microsoft/coreutils) is a
//! Microsoft-maintained build of [uutils/coreutils](https://github.com/uutils/coreutils)
//! that also bundles `findutils` (`find`, `xargs`) and a GNU-compatible
//! `grep`. It ships as one multi-call binary with a hardlink per utility, so
//! both invocation forms exist:
//!
//! ```text
//! ls -l              # via the per-utility hardlink
//! coreutils ls -l    # via the multi-call binary
//! ```
//!
//! The same binary is what `uutils/coreutils` installs on Linux and macOS, so
//! this module is not Windows-specific.
//!
//! What we do when it is installed:
//!
//!  1. Register a `coreutils` spec whose subcommands are the utilities the
//!     installed binary actually reports (`coreutils --list`), each delegating
//!     to the same-named spec so `coreutils ls --<TAB>` completes `ls`'s
//!     options.
//!  2. Synthesize a spec for any utility the bundled corpus does not cover
//!     (`b2sum`, `numfmt`, `sha256sum`, …) by parsing its `--help`. uutils is
//!     built on `clap`, so the help layout is regular enough to parse.
//!
//! Everything is discovered at runtime from the installed binary rather than
//! hardcoded, because Microsoft's fork excludes some utilities (`timeout`,
//! `kill`, `chmod` collide with Windows built-ins) and adds others.
//!
//! Set `INSH_RS_NO_COREUTILS=1` to skip detection entirely.

use crate::spec::model::{Arg, LoadSpec, Opt, Subcommand, Template};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

/// How long any single `coreutils` probe may take before we give up on it.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// An installed multi-call coreutils binary and the utilities it provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coreutils {
    pub binary: PathBuf,
    /// Utility names, sorted, exactly as the binary reports them.
    pub utils: Vec<String>,
}

/// Locate the installed coreutils binary and enumerate its utilities.
///
/// Probed once per process. `None` when it is not installed, when detection is
/// disabled, or when the binary does not answer `--list` (i.e. it is some
/// unrelated program that happens to be named `coreutils`).
pub fn detect() -> Option<&'static Coreutils> {
    static DETECTED: OnceLock<Option<Coreutils>> = OnceLock::new();
    DETECTED.get_or_init(probe).as_ref()
}

fn probe() -> Option<Coreutils> {
    if crate::env::coreutils_disabled() {
        return None;
    }
    let binary = PathBuf::from(crate::platform::find_on_path("coreutils")?);
    let utils = list_utils(&binary)?;
    if utils.is_empty() {
        return None;
    }
    Some(Coreutils { binary, utils })
}

/// `coreutils --list`, memoized on disk.
///
/// The list only changes when the binary does, so it is cached under a key
/// derived from the binary's size and mtime. Without the cache every
/// `is complete` invocation would spawn a subprocess before answering.
fn list_utils(binary: &Path) -> Option<Vec<String>> {
    let cache = cache_dir(binary).map(|d| d.join("list.txt"));
    if let Some(path) = &cache
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let utils = parse_list(&text);
        if !utils.is_empty() {
            return Some(utils);
        }
    }

    let Probe::Output(text) = run_capture(binary, &["--list"]) else {
        return None;
    };
    let utils = parse_list(&text);
    if utils.is_empty() {
        // Not a uutils multi-call binary.
        return None;
    }
    if let Some(path) = &cache {
        persist(path, text.as_bytes());
    }
    Some(utils)
}

fn parse_list(text: &str) -> Vec<String> {
    let mut utils: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .map(str::to_string)
        .collect();
    utils.sort();
    utils.dedup();
    utils
}

/// Persist `bytes` to `path`, creating parent directories as needed.
///
/// The cache is written while a completion is already running, so a plain
/// truncate-and-write could leave torn content behind if the process is killed
/// or the disk fills mid-write — and readers would then serve that fragment as
/// authoritative until the binary's fingerprint changed. Write to a sibling
/// temp file and rename instead: a reader sees either the previous content or
/// the complete new one. Best-effort; the cache can always be rebuilt.
fn persist(path: &Path, bytes: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(name) = path.file_name() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // The pid keeps two concurrent `is` processes from writing the same temp
    // file at once.
    let tmp = parent.join(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Answer `owns()` for a probe verdict, remembering it under `cache` only when
/// the verdict was actually measured.
///
/// A probe that came up empty — a timeout above all, which is exactly what a
/// cold start on a machine with an antivirus produces — says nothing about
/// which implementation the binary on PATH is. Caching that as
/// "not ours" would disable option augmentation for every following
/// invocation until the coreutils binary's fingerprint changed.
fn remember(cache: Option<&Path>, resolved: &str, verdict: Option<bool>) -> bool {
    match verdict {
        Some(owned) => {
            if let Some(path) = cache {
                persist(path, format!("{resolved}\n{}", u8::from(owned)).as_bytes());
            }
            owned
        }
        None => false,
    }
}

/// `~/.inshellisense/coreutils/<fingerprint>/`, keyed by the binary's identity
/// so an upgraded coreutils re-probes instead of serving a stale utility list.
fn cache_dir(binary: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(binary).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for bytes in [
        binary.to_string_lossy().as_bytes(),
        &meta.len().to_le_bytes()[..],
        &mtime.to_le_bytes()[..],
    ] {
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Some(
        crate::paths::resource_root()?
            .join("coreutils")
            .join(format!("{hash:016x}")),
    )
}

/// Run `binary <args>` and capture stdout, bounded by [`PROBE_TIMEOUT`].
fn run_capture(binary: &Path, args: &[&str]) -> Probe {
    run_capture_within(binary, args, PROBE_TIMEOUT)
}

/// The outcome of one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// Complete stdout from a child that finished within the budget.
    Output(String),
    /// The child outlived the budget and was killed. Whatever that says about
    /// the binary, it is transient — a cold cache, a scanning antivirus — and
    /// must not be remembered as an answer about the implementation.
    Timeout,
    /// The child could not be spawned or its output could not be read.
    Failed,
}

fn run_capture_within(binary: &Path, args: &[&str], timeout: Duration) -> Probe {
    let mut child = match Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Probe::Failed,
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Probe::Failed;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        let _ = tx.send(buf);
    });
    let bytes = match rx.recv_timeout(timeout) {
        Ok(bytes) => bytes,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Probe::Timeout;
        }
    };
    let _ = child.wait();
    Probe::Output(String::from_utf8_lossy(&bytes).into_owned())
}

impl Coreutils {
    pub fn provides(&self, util: &str) -> bool {
        self.utils.iter().any(|u| u == util)
    }

    /// The `coreutils` multi-call spec itself. Each utility becomes a
    /// subcommand that delegates to the spec of the same name, so the bundled
    /// Fig spec for `ls` drives `coreutils ls --<TAB>`.
    pub fn root_spec(&self) -> Subcommand {
        let mut spec = Subcommand::new("coreutils");
        spec.description = Some("Multi-call binary for the coreutils utilities".into());
        spec.options = vec![
            named_opt(&["--list"], "List all defined functions, one per row"),
            named_opt(&["-h", "--help"], "Print help"),
            named_opt(&["-V", "--version"], "Print version"),
        ];
        spec.subcommands = self
            .utils
            .iter()
            .map(|util| {
                let mut sub = Subcommand::new(util);
                sub.load_spec = Some(LoadSpec::SpecPath { name: util.clone() });
                sub
            })
            .collect();
        spec
    }

    /// Is the `util` a shell would actually run *this* coreutils build?
    ///
    /// Branding is not a safe discriminator — Microsoft's fork need not call
    /// itself "uutils". Instead compare the version banner of the binary on
    /// `PATH` against the one the multi-call binary prints for the same
    /// utility. Identical output means identical implementation, which is what
    /// licenses us to describe one using the other's `--help`.
    ///
    /// Answered once per (binary, resolved path) and cached on disk.
    pub fn owns(&self, util: &str) -> bool {
        if !self.provides(util) {
            return false;
        }
        let Some(resolved) = crate::platform::find_on_path(util) else {
            return false;
        };
        let cache = cache_dir(&self.binary).map(|d| d.join("owned").join(sanitize(util)));
        if let Some(path) = &cache
            && let Ok(text) = std::fs::read_to_string(path)
            && let Some((cached_path, flag)) = text.split_once('\n')
            && cached_path == resolved
        {
            return flag.trim() == "1";
        }

        remember(
            cache.as_deref(),
            &resolved,
            self.same_implementation(Path::new(&resolved), util),
        )
    }

    /// Did the binary on PATH print the same version banner as this build for
    /// `util`? `None` when either probe produced no output — a timed-out or
    /// failed probe is not evidence that the implementations differ.
    fn same_implementation(&self, resolved: &Path, util: &str) -> Option<bool> {
        let Probe::Output(on_path) = run_capture(resolved, &["--version"]) else {
            return None;
        };
        let Probe::Output(ours) = run_capture(&self.binary, &[util, "--version"]) else {
            return None;
        };
        let banner = first_line(&on_path);
        Some(!banner.is_empty() && banner == first_line(&ours))
    }

    /// Add every option the installed binary accepts that `spec` does not
    /// already describe, and give it a path operand if it has none.
    ///
    /// The bundled corpus is Fig's, which is BSD/macOS-flavored: its `ls` spec
    /// knows 41 short flags but only one long option. A coreutils `ls` accepts
    /// `--all`, `--human-readable` and the rest, so on a system where coreutils
    /// *is* `ls`, those must complete. Merging rather than replacing keeps
    /// Fig's curated descriptions and generators.
    pub fn augment(&self, spec: &mut Subcommand) {
        let name = spec.name().to_string();
        let Some(synthesized) = self.spec_for(&name) else {
            return;
        };
        merge_options(spec, synthesized.options);
        if spec.args.is_empty() {
            spec.args = synthesized.args;
        }
    }

    /// A spec for `util`, synthesized from `coreutils <util> --help` and cached
    /// on disk. Used for utilities the bundled corpus does not cover, and as
    /// the source of extra options for those it does.
    pub fn spec_for(&self, util: &str) -> Option<Subcommand> {
        if !self.provides(util) {
            return None;
        }
        let cache = cache_dir(&self.binary).map(|d| d.join(format!("{}.json", sanitize(util))));
        if let Some(path) = &cache
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(spec) = serde_json::from_slice::<Subcommand>(&bytes)
        {
            return Some(spec);
        }

        // Go through the multi-call binary rather than the per-utility
        // hardlink: the hardlinks may not be on PATH. Nothing captured — a
        // timeout, or an empty banner — yields no spec and caches nothing,
        // so the next invocation probes afresh instead of serving a stub.
        let Probe::Output(help) = run_capture(&self.binary, &[util, "--help"]) else {
            return None;
        };
        if help.trim().is_empty() {
            return None;
        }
        let spec = parse_help(util, &help);
        if let Some(path) = &cache
            && let Ok(json) = serde_json::to_vec(&spec)
        {
            persist(path, &json);
        }
        Some(spec)
    }
}

/// Fold `extra` into `spec.options`.
///
/// An option that shares *any* name with an existing one contributes only its
/// unknown names. Fig's `ls` spec knows `-a` but not its long form, so
/// skipping the whole option would have lost `--all`; replacing it would have
/// lost the curated description and the argument semantics the resolver
/// depends on.
fn merge_options(spec: &mut Subcommand, extra: Vec<Opt>) {
    for opt in extra {
        let existing = spec
            .options
            .iter_mut()
            .find(|existing| opt.names.iter().any(|n| existing.names.contains(n)));
        match existing {
            Some(existing) => {
                for name in opt.names {
                    if !existing.names.contains(&name) {
                        existing.names.push(name);
                    }
                }
            }
            None => spec.options.push(opt),
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// A utility name may be `[` (the `test` builtin), which is not a filename.
fn sanitize(util: &str) -> String {
    util.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn named_opt(names: &[&str], description: &str) -> Opt {
    let mut opt = Opt::new(names);
    opt.description = Some(description.to_string());
    opt
}

/// Parse `clap`-style `--help` output into a spec.
///
/// The layout uutils emits is:
///
/// ```text
/// Print or check the BLAKE2b checksums
///
/// Usage: b2sum [OPTIONS] [FILE]...
///
/// Options:
///   -c, --check            read checksums from the FILEs and check them
///       --status           don't output anything, status code shows success
///   -l, --length <length>  digest length in bits; must not exceed the max size
///                          and must be a multiple of 8
/// ```
///
/// An option line's first non-blank character is `-`; anything else indented
/// under one is a continuation of its description. Prose after the options
/// block starts at column zero, which is how the block's end is detected.
pub fn parse_help(name: &str, help: &str) -> Subcommand {
    let mut spec = Subcommand::new(name);
    let mut section = Section::Preamble;
    let mut options: Vec<Opt> = Vec::new();
    let mut usage = String::new();
    let mut option_indent: Option<usize> = None;

    for line in help.lines() {
        let trimmed = line.trim();

        // Section headers sit at column zero.
        if !line.starts_with(' ') && trimmed.ends_with(':') {
            section = match trimmed {
                "Options:" => Section::Options,
                "Arguments:" => Section::Arguments,
                _ => Section::Other,
            };
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Usage:") {
            if usage.is_empty() {
                usage = rest.trim().to_string();
            }
            section = Section::Other;
            continue;
        }

        match section {
            Section::Preamble => {
                // The first non-empty line is the description.
                if spec.description.is_none() && !trimmed.is_empty() {
                    spec.description = Some(trimmed.to_string());
                }
            }
            Section::Options => {
                if trimmed.is_empty() {
                    continue;
                }
                if !line.starts_with("  ") {
                    // Trailing prose (see `sleep --help`) ends the block.
                    section = Section::Other;
                    continue;
                }
                if starts_option_line(line, option_indent) {
                    let indent = indent_of(line);
                    if option_indent.is_none_or(|seen| indent < seen) {
                        option_indent = Some(indent);
                    }
                    if let Some(opt) = parse_option_line(trimmed) {
                        options.push(opt);
                    }
                } else if let Some(last) = options.last_mut() {
                    append_description(last, trimmed);
                }
            }
            Section::Arguments | Section::Other => {}
        }
    }

    spec.options = options;
    spec.args = usage_args(&usage);
    spec
}

enum Section {
    Preamble,
    Options,
    Arguments,
    Other,
}

/// Does `line` begin a new option rather than continue the previous one?
///
/// Wrapped descriptions routinely start with a dash — uutils `cp` continues
/// `--remove-destination` with "`--force). On Windows, …`" and `pr` continues
/// `--sep-string` with "`-J and \`<space>\``" — so "starts with -" is not
/// enough. A real option line leads with a token shaped like a flag and sits
/// at (or near) the column the section's other option lines use: prose hangs
/// deeper, under the description column.
fn starts_option_line(line: &str, option_indent: Option<usize>) -> bool {
    // `-c, --check` leads with `-c,`; the comma belongs to the flag.
    let token = line
        .split([',', ' ', '\t'])
        .find(|t| !t.is_empty())
        .unwrap_or("");
    is_flag_spec(token)
        && option_indent.is_none_or(|seen| indent_of(line) <= seen + OPTION_INDENT_SLACK)
}

/// How much deeper than the shallowest option line an option may still start:
/// GNU/clap pad long-only flags to `  -x, ` width (2 → 6).
const OPTION_INDENT_SLACK: usize = 4;

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Is `token` shaped like a flag (`-x`, `--long`, `--multi-word`), possibly
/// with a value part attached? Prose fragments (`--force).`) are not.
fn is_flag_spec(token: &str) -> bool {
    let Some(body) = token.strip_prefix("--").or_else(|| token.strip_prefix('-')) else {
        return false;
    };
    // The flag name ends at a value delimiter; what follows is the value
    // (`=<SIZE>`, `[=<WHEN>]`) and is judged by `is_value_placeholder`.
    let name = body.split(['=', '[', '<', ' ']).next().unwrap_or("");
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Is `value` a placeholder naming the option's argument (`<file>`,
/// `[=<when>]`, `SIZE`) rather than the start of a sentence that happens to
/// follow the flag ("On Windows", "and `<space>`")?
fn is_value_placeholder(value: &str) -> bool {
    let value = value.trim().trim_start_matches('=');
    if value.starts_with('<') || value.starts_with('[') {
        return true;
    }
    // GNU style writes bare all-caps placeholders: `-S STRING`, `-i LO-HI`.
    let word = value.split_whitespace().next().unwrap_or("");
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c == '-' || c.is_ascii_digit())
}

/// `-l, --length <length>  digest length in bits` → names, value, description.
fn parse_option_line(line: &str) -> Option<Opt> {
    // The description begins at the first run of two or more spaces.
    let (flags, description) = match find_gap(line) {
        Some(idx) => (line[..idx].trim(), line[idx..].trim()),
        None => (line.trim(), ""),
    };

    let mut names = Vec::new();
    let mut takes_value: Option<(String, bool)> = None;
    for token in flags.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        // `--length <length>`, `--block-size=<SIZE>`, `--hyperlink[=<WHEN>]`
        let (flag, value) = split_flag(token);
        if !is_flag_spec(flag) {
            continue;
        }
        names.push(flag.to_string());
        if let Some(value) = value
            && is_value_placeholder(value)
        {
            // A bracketed value is optional (`--color[=<when>]`). Getting this
            // wrong would make the resolver eat the following filename as the
            // option's value.
            let optional = value.starts_with('[');
            takes_value = Some((strip_brackets(value).to_string(), optional));
        }
    }
    if names.is_empty() {
        return None;
    }

    let mut opt = Opt::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
    if !description.is_empty() {
        opt.description = Some(description.to_string());
    }
    if let Some((value, optional)) = takes_value {
        opt.args = vec![Arg {
            name: Some(value),
            is_optional: optional,
            ..Default::default()
        }];
    }
    Some(opt)
}

/// Split `--hyperlink[=<WHEN>]` into `("--hyperlink", Some("[=<WHEN>]"))`.
fn split_flag(token: &str) -> (&str, Option<&str>) {
    match token.find([' ', '=', '[']) {
        Some(idx) => (token[..idx].trim(), Some(token[idx..].trim())),
        None => (token, None),
    }
}

/// Index of the first run of two or more spaces — the column where clap starts
/// the description.
fn find_gap(line: &str) -> Option<usize> {
    line.as_bytes()
        .windows(2)
        .position(|w| w == b"  ")
        .filter(|idx| *idx > 0)
}

fn strip_brackets(value: &str) -> &str {
    value
        .trim()
        .trim_start_matches(['<', '[', '{', '='])
        .trim_end_matches(['>', ']', '}', '.'])
}

fn append_description(opt: &mut Opt, continuation: &str) {
    // `[default: 1]` and friends add nothing to a completion popup.
    if continuation.starts_with('[') {
        return;
    }
    match &mut opt.description {
        Some(existing) => {
            existing.push(' ');
            existing.push_str(continuation);
        }
        None => opt.description = Some(continuation.to_string()),
    }
}

/// Derive positional args from the `Usage:` line, so `b2sum <TAB>` completes
/// paths. Only the operand kinds coreutils actually uses are recognized.
fn usage_args(usage: &str) -> Vec<Arg> {
    let upper = usage.to_uppercase();
    let (template, name) = if upper.contains("DIRECTORY") {
        (Template::Folders, "DIRECTORY")
    } else if upper.contains("FILE") {
        (Template::Filepaths, "FILE")
    } else {
        return Vec::new();
    };
    vec![Arg {
        name: Some(name.to_string()),
        templates: vec![template],
        is_variadic: usage.contains("..."),
        is_optional: true,
        ..Default::default()
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    const B2SUM: &str = "\
Print or check the BLAKE2b checksums

Usage: b2sum [OPTIONS] [FILE]...

Options:
  -c, --check            read checksums from the FILEs and check them
      --status           don't output anything, status code shows success
  -l, --length <length>  digest length in bits; must not exceed the max size
                         and must be a multiple of 8 for blake2b
      --tag              create a BSD style checksum
  -h, --help             Print help
";

    const SLEEP: &str = "\
Pause for NUMBER seconds.

Usage: sleep NUMBER[SUFFIX]...
       sleep OPTION

Arguments:
  [NUMBER]...  pause for NUMBER seconds

Options:
  -h, --help     Print help
  -V, --version  Print version

Pause for NUMBER seconds. SUFFIX may be 's' for seconds (the default),
'm' for minutes, 'h' for hours or 'd' for days.
";

    const NUMFMT: &str = "\
Convert numbers from/to human-readable strings

Usage: numfmt [OPTION]... [NUMBER]...

Options:
      --debug                    print warnings about invalid input
  -d, --delimiter <X>            use X instead of whitespace for field delimiter
      --field <FIELDS>           replace the numbers in these input fields
                                 [default: 1]
";

    #[test]
    fn parses_description_options_and_values() {
        let spec = parse_help("b2sum", B2SUM);
        assert_eq!(spec.name(), "b2sum");
        assert_eq!(
            spec.description.as_deref(),
            Some("Print or check the BLAKE2b checksums")
        );

        let names: Vec<&str> = spec.options.iter().map(|o| o.names[0].as_str()).collect();
        assert_eq!(names, ["-c", "--status", "-l", "--tag", "-h"]);

        let check = &spec.options[0];
        assert_eq!(check.names, ["-c", "--check"]);
        assert!(check.args.is_empty());

        // A value placeholder becomes the option's argument.
        let length = &spec.options[2];
        assert_eq!(length.names, ["-l", "--length"]);
        assert_eq!(length.args[0].name.as_deref(), Some("length"));
    }

    /// A wrapped description continues on the next line and must not be read
    /// as another option.
    #[test]
    fn continuation_lines_extend_the_description() {
        let spec = parse_help("b2sum", B2SUM);
        let length = &spec.options[2];
        let desc = length.description.as_deref().unwrap();
        assert!(desc.starts_with("digest length in bits"));
        assert!(desc.ends_with("must be a multiple of 8 for blake2b"));
    }

    /// `[default: 1]` is noise in a completion popup.
    #[test]
    fn default_annotations_are_dropped() {
        let spec = parse_help("numfmt", NUMFMT);
        let field = spec
            .options
            .iter()
            .find(|o| o.names.contains(&"--field".to_string()))
            .unwrap();
        assert_eq!(
            field.description.as_deref(),
            Some("replace the numbers in these input fields")
        );
        assert_eq!(field.args[0].name.as_deref(), Some("FIELDS"));
    }

    /// Prose after the options block starts at column zero and must end it —
    /// otherwise `sleep`'s trailing paragraph is parsed as options.
    #[test]
    fn trailing_prose_ends_the_options_block() {
        let spec = parse_help("sleep", SLEEP);
        let names: Vec<&str> = spec.options.iter().map(|o| o.names[0].as_str()).collect();
        assert_eq!(names, ["-h", "-V"]);
        assert!(spec.args.is_empty(), "sleep takes no FILE operand");
    }

    /// `[FILE]...` in the usage line gives path completion.
    #[test]
    fn usage_line_yields_a_filepath_operand() {
        let spec = parse_help("b2sum", B2SUM);
        assert_eq!(spec.args.len(), 1);
        assert_eq!(spec.args[0].name.as_deref(), Some("FILE"));
        assert!(spec.args[0].is_variadic);
        assert!(spec.args[0].is_optional);
        assert_eq!(spec.args[0].templates, vec![Template::Filepaths]);
    }

    #[test]
    fn root_spec_delegates_each_utility_to_its_own_spec() {
        let cu = Coreutils {
            binary: PathBuf::from("/usr/bin/coreutils"),
            utils: vec!["ls".into(), "b2sum".into()],
        };
        let spec = cu.root_spec();
        assert_eq!(spec.name(), "coreutils");
        assert!(spec.options.iter().any(|o| o.names[0] == "--list"));
        assert_eq!(spec.subcommands.len(), 2);
        let ls = &spec.subcommands[0];
        assert_eq!(ls.name(), "ls");
        match &ls.load_spec {
            Some(LoadSpec::SpecPath { name }) => assert_eq!(name, "ls"),
            other => panic!("expected SpecPath, got {other:?}"),
        }
        assert!(cu.provides("b2sum"));
        assert!(!cu.provides("nope"));
    }

    const LS_SHAPES: &str = "\
List directory contents.

Usage: ls [OPTION]... [FILE]...

Options:
      --hyperlink[=<WHEN>]
          hyperlink file names WHEN [default: never]
  -a, --all
          do not ignore entries starting with .
  -h, --human-readable
          print sizes like 1K 234M 2G
      --block-size=<BLOCK_SIZE>
          scale sizes by BLOCK_SIZE
";

    /// clap emits `--flag[=<VALUE>]`, `--flag=<VALUE>` and `--flag <VALUE>`.
    /// Splitting only on space/`=` produced the name `--hyperlink[`.
    #[test]
    fn value_syntaxes_do_not_leak_into_the_option_name() {
        let spec = parse_help("ls", LS_SHAPES);
        let names: Vec<&str> = spec.options.iter().map(|o| o.names[0].as_str()).collect();
        assert_eq!(names, ["--hyperlink", "-a", "-h", "--block-size"]);
        assert!(
            spec.options.iter().all(|o| o
                .names
                .iter()
                .all(|n| !n.contains(['[', ']', '<', '>', '=']))),
            "a bracket leaked into an option name"
        );
    }

    /// A bracketed value is optional. Marking it required would make the
    /// resolver consume the following filename as `--color`'s value.
    #[test]
    fn bracketed_values_are_optional_and_bare_ones_are_not() {
        let spec = parse_help("ls", LS_SHAPES);
        let hyperlink = &spec.options[0];
        assert_eq!(hyperlink.args[0].name.as_deref(), Some("WHEN"));
        assert!(hyperlink.args[0].is_optional);

        let block_size = &spec.options[3];
        assert_eq!(block_size.args[0].name.as_deref(), Some("BLOCK_SIZE"));
        assert!(!block_size.args[0].is_optional);
    }

    /// clap wraps long help onto the next line; that line is the option's
    /// description, not another option.
    #[test]
    fn description_on_the_following_line_is_attached() {
        let spec = parse_help("ls", LS_SHAPES);
        let all = &spec.options[1];
        assert_eq!(all.names, ["-a", "--all"]);
        assert_eq!(
            all.description.as_deref(),
            Some("do not ignore entries starting with .")
        );
    }

    fn opt_with(names: &[&str], description: &str) -> Opt {
        let mut opt = Opt::new(names);
        opt.description = Some(description.to_string());
        opt
    }

    /// Fig's `ls` spec knows `-a` but not `--all`. Skipping any option that
    /// shares a name would have dropped `--all` entirely; folding the unknown
    /// names into the existing option keeps the curated description.
    #[test]
    fn augment_folds_long_forms_into_existing_short_options() {
        let mut bundled = Subcommand::new("ls");
        bundled.options = vec![opt_with(
            &["-a"],
            "Include directory entries whose names begin with a dot",
        )];

        // Stand in for `spec_for`, which would spawn the real binary.
        let synthesized = parse_help("ls", LS_SHAPES);
        merge_options(&mut bundled, synthesized.options);

        assert_eq!(bundled.options.len(), 4, "three new options added");
        let a = &bundled.options[0];
        assert_eq!(a.names, ["-a", "--all"], "long form folded in");
        assert_eq!(
            a.description.as_deref(),
            Some("Include directory entries whose names begin with a dot"),
            "curated description preserved"
        );
        assert!(a.args.is_empty(), "curated argument semantics preserved");

        // Re-merging is idempotent — no duplicate names.
        merge_options(&mut bundled, parse_help("ls", LS_SHAPES).options);
        assert_eq!(bundled.options.len(), 4);
        assert_eq!(bundled.options[0].names, ["-a", "--all"]);
    }

    #[test]
    fn version_banner_comparison_uses_the_first_line() {
        assert_eq!(
            first_line("ls (uutils coreutils) 0.9.0\nmore\n"),
            "ls (uutils coreutils) 0.9.0"
        );
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn list_output_is_parsed_and_sorted() {
        let utils = parse_list("ls\nb2sum\n\n  cat  \nnot a util\n");
        assert_eq!(utils, ["b2sum", "cat", "ls"]);
    }

    /// `[` is a valid utility name and not a valid filename component.
    #[test]
    fn cache_filenames_are_sanitized() {
        assert_eq!(sanitize("["), "_");
        assert_eq!(sanitize("sha256sum"), "sha256sum");
    }

    /// Real uutils 0.10 `cp --help` output: the `--remove-destination`
    /// description wraps onto a line starting with "`--force).`", which used
    /// to be parsed as an option named `--force).` requiring the value "On
    /// Windows" — and persisted into the cache.
    const CP: &str = "\
Usage: cp [OPTION]... [-T] SOURCE DEST

Options:
  -t, --target-directory <target-directory>
          copy all SOURCE arguments into target-directory
  -f, --force
          if an existing destination file cannot be opened, remove it and try again (this option is
          ignored when the -n option is also used). Currently not implemented for Windows.
      --remove-destination
          remove each existing destination file before attempting to open it (contrast with
          --force). On Windows, currently only works for writeable files.
      --backup[=<CONTROL>]
          make a backup of each existing destination file
";

    /// Real uutils 0.10 `pr --help` output: the `--sep-string` description
    /// wraps onto "`-J and \`<space>\``", which used to manufacture a second,
    /// value-taking `-J` that shadowed the real one.
    const PR: &str = r#"Options:
  -S, --sep-string [<string>]           separate columns by STRING,
                                                        without -S: Default separator `<TAB>` with
                                                        -J and `<space>`
                                                        otherwise (same as -S" "), no effect on
                                                        column options
  -J                                    merge full lines, turns off -W line truncation, no column
                                                        alignment, --sep-string[=STRING] sets
                                                        separators
      --help                            Print help information
"#;

    /// A wrapped description that begins with a dash is not an option (#57).
    #[test]
    fn wrapped_prose_starting_with_a_dash_is_not_an_option() {
        let spec = parse_help("cp", CP);
        let names: Vec<&str> = spec
            .options
            .iter()
            .flat_map(|o| o.names.iter().map(String::as_str))
            .collect();
        assert!(
            names.iter().all(|name| !name.contains([')', '.'])),
            "manufactured option from wrapped prose: {names:?}"
        );
        // The prose stays where it belongs — in the description it continues.
        let remove = spec
            .options
            .iter()
            .find(|o| o.names.contains(&"--remove-destination".to_string()))
            .unwrap();
        assert!(
            remove
                .description
                .as_deref()
                .unwrap_or_default()
                .ends_with("--force). On Windows, currently only works for writeable files.")
        );
    }

    #[test]
    fn cp_options_keep_their_values_and_defaults() {
        let spec = parse_help("cp", CP);
        let force = spec
            .options
            .iter()
            .find(|o| o.names.contains(&"--force".to_string()))
            .unwrap();
        assert!(force.args.is_empty(), "--force takes no value");

        let backup = spec
            .options
            .iter()
            .find(|o| o.names.contains(&"--backup".to_string()))
            .unwrap();
        assert_eq!(backup.args[0].name.as_deref(), Some("CONTROL"));
        assert!(backup.args[0].is_optional);
    }

    #[test]
    fn a_mid_sentence_flag_mention_does_not_shadow_the_real_option() {
        let spec = parse_help("pr", PR);
        let js: Vec<&Opt> = spec
            .options
            .iter()
            .filter(|o| o.names.contains(&"-J".to_string()))
            .collect();
        assert_eq!(js.len(), 1, "one -J, not the sentence fragment");
        assert!(js[0].args.is_empty(), "-J takes no value");
        assert_eq!(
            js[0].description.as_deref(),
            Some(
                "merge full lines, turns off -W line truncation, no column alignment, --sep-string[=STRING] sets separators"
            )
        );
    }

    /// GNU-style bare uppercase placeholders still mark a value-taking option.
    #[test]
    fn bare_uppercase_placeholders_take_a_value() {
        assert!(is_value_placeholder("<FILE>"));
        assert!(is_value_placeholder("[=<WHEN>]"));
        assert!(is_value_placeholder("=<SIZE>"));
        assert!(is_value_placeholder("SIZE"));
        assert!(!is_value_placeholder("and `<space>`"));
        assert!(!is_value_placeholder("On Windows"));
    }

    #[test]
    fn flag_shapes_are_distinguished_from_prose() {
        assert!(is_flag_spec("-c"));
        assert!(is_flag_spec("-J"));
        assert!(is_flag_spec("--all"));
        assert!(is_flag_spec("--sep-string"));
        assert!(is_flag_spec("--hyperlink[=<WHEN>]"));
        assert!(is_flag_spec("--block-size=<SIZE>"));
        assert!(!is_flag_spec("--force)."));
        assert!(!is_flag_spec("--"));
        assert!(!is_flag_spec("-S\""));
        assert!(!is_flag_spec("hyperlink"));
    }

    /// A crash mid-write must never leave torn content behind that a reader
    /// would take as authoritative: the payload lands via temp file + rename
    /// (#69).
    #[test]
    fn cached_files_are_written_atomically() {
        let dir = crate::test_support::unique_temp_dir("cache");
        let path = dir.join("fp").join("list.txt");
        persist(&path, b"ls\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ls\n");

        // Replacing existing content works, and no temp file survives.
        persist(&path, b"ls\ncat\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ls\ncat\n");
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp file left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A verdict that was not measured must not be cached: a probe that timed
    /// out would otherwise be remembered forever as owns() == false, disabling
    /// option augmentation until the coreutils binary changed (#70).
    #[test]
    fn an_unmeasured_verdict_is_not_cached() {
        let dir = crate::test_support::unique_temp_dir("owns");
        let path = dir.join("owned").join("ls");
        persist(&path, b"/usr/bin/ls\n1");

        // No verdict: the previous measurement stays on disk and is untouched.
        assert!(!remember(Some(&path), "/usr/bin/ls", None));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/usr/bin/ls\n1");

        // A real measurement, in either direction, is recorded.
        assert!(!remember(Some(&path), "/usr/bin/ls", Some(false)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/usr/bin/ls\n0");
        assert!(remember(Some(&path), "/usr/bin/ls", Some(true)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/usr/bin/ls\n1");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Probes that cannot spawn yield no verdict at all.
    #[test]
    fn a_failed_probe_yields_no_verdict() {
        let cu = Coreutils {
            binary: PathBuf::from("/nonexistent/coreutils"),
            utils: vec!["ls".into()],
        };
        assert_eq!(
            cu.same_implementation(Path::new("/nonexistent/ls"), "ls"),
            None
        );
    }

    /// The timeout is distinguishable from a completed probe — that
    /// distinction is what keeps it out of the cache.
    #[cfg(unix)]
    #[test]
    fn a_slow_probe_reports_a_timeout() {
        let dir = crate::test_support::unique_temp_dir("probe");
        let script = dir.join("slow");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = fast ]; then echo banner; else sleep 5; fi\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            run_capture_within(&script, &["slow"], Duration::from_millis(100)),
            Probe::Timeout
        );
        assert_eq!(
            run_capture_within(&script, &["fast"], Duration::from_millis(5000)),
            Probe::Output("banner\n".into())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
