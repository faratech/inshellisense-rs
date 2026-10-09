//! Parity scanner — systematic divergence detection between inshellisense-rs
//! and upstream inshellisense.
//!
//! For each category (cli, init, doctor, complete, specs, render), run the
//! same corpus of inputs through both binaries, normalize their outputs into
//! a common shape, and emit a markdown report that ranks divergences by user
//! impact.

pub mod cli;
pub mod complete;
pub mod doctor;
pub mod init;
#[cfg(unix)]
pub mod render;
pub mod report;
pub mod specs;

use std::path::PathBuf;

/// All scanner categories. Each has a corresponding module with a
/// `run(&ScanConfig) -> CategoryReport` entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Cli,
    Init,
    Doctor,
    Complete,
    Specs,
    #[cfg(unix)]
    Render,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Cli => "cli",
            Category::Init => "init",
            Category::Doctor => "doctor",
            Category::Complete => "complete",
            Category::Specs => "specs",
            #[cfg(unix)]
            Category::Render => "render",
        }
    }

    pub fn all() -> Vec<Category> {
        #[allow(unused_mut)]
        let mut v = vec![
            Category::Cli,
            Category::Init,
            Category::Doctor,
            Category::Complete,
            Category::Specs,
        ];
        #[cfg(unix)]
        v.push(Category::Render);
        v
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "cli" => Some(Category::Cli),
            "init" => Some(Category::Init),
            "doctor" => Some(Category::Doctor),
            "complete" => Some(Category::Complete),
            "specs" => Some(Category::Specs),
            #[cfg(unix)]
            "render" => Some(Category::Render),
            _ => None,
        }
    }
}

/// One tested input's result.
#[derive(Debug, Clone)]
pub enum CaseResult {
    Pass,
    Fail {
        reason: String,
        details: Vec<String>,
    },
}

impl CaseResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, CaseResult::Pass)
    }
}

#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    pub result: CaseResult,
    /// Impact ranking (0 = least important, 100 = most). Used to sort
    /// the top-10 divergences list in the report.
    pub impact: u8,
}

#[derive(Debug, Clone)]
pub struct CategoryReport {
    pub category: Category,
    pub cases: Vec<Case>,
    /// Free-form summary blurb shown in the Details column.
    pub summary: String,
}

impl CategoryReport {
    pub fn new(category: Category) -> Self {
        Self {
            category,
            cases: Vec::new(),
            summary: String::new(),
        }
    }

    pub fn push_pass(&mut self, name: impl Into<String>) {
        self.cases.push(Case {
            name: name.into(),
            result: CaseResult::Pass,
            impact: 0,
        });
    }

    pub fn push_fail(
        &mut self,
        name: impl Into<String>,
        reason: impl Into<String>,
        details: Vec<String>,
        impact: u8,
    ) {
        self.cases.push(Case {
            name: name.into(),
            result: CaseResult::Fail {
                reason: reason.into(),
                details,
            },
            impact,
        });
    }

    pub fn pass_count(&self) -> usize {
        self.cases.iter().filter(|c| c.result.is_pass()).count()
    }

    pub fn fail_count(&self) -> usize {
        self.cases.len() - self.pass_count()
    }

    /// A category that ran no cases proved nothing. Scoring it 1.0 meant a
    /// deleted corpus, an unreadable corpus, or an all-malformed corpus
    /// silently reported perfect parity and passed the threshold.
    pub fn score(&self) -> f64 {
        if self.cases.is_empty() {
            return 0.0;
        }
        self.pass_count() as f64 / self.cases.len() as f64
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub categories: Vec<CategoryReport>,
}

impl Report {
    pub fn new() -> Self {
        Self {
            categories: Vec::new(),
        }
    }

    pub fn total_failures(&self) -> usize {
        self.categories.iter().map(|c| c.fail_count()).sum()
    }

    /// A report with no categories has no minimum to clear — treat it as a
    /// failure rather than folding to a vacuous 1.0.
    pub fn min_score(&self) -> f64 {
        if self.categories.is_empty() {
            return 0.0;
        }
        self.categories
            .iter()
            .map(|c| c.score())
            .fold(f64::INFINITY, f64::min)
    }
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

/// Runtime configuration for one scan invocation.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub ours: PathBuf,
    pub upstream: PathBuf,
    pub corpus_dir: PathBuf,
    pub output_path: PathBuf,
    pub raw_dir: PathBuf,
    pub categories: Vec<Category>,
    pub verbose: bool,
    pub threshold: f64,
}

/// Create the scan's output directory, or reuse it only if it is private.
///
/// Raw captures are written into it with `fs::write`, and the scratch HOMEs
/// whose startup files the spawned shells source live inside it. The
/// defaults used to be fixed names in the shared `/tmp`, accepted with
/// `create_dir_all` whoever had created them: another local user could
/// pre-create `/tmp/parity-out`, plant a `.bashrc` in a scratch HOME (run as
/// the developer) or a symlink where a capture is written. On Unix the
/// directory is created 0700 and refused if it is a symlink, owned by
/// another user, or writable by group or others.
pub fn prepare_output_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let meta = std::fs::symlink_metadata(dir)?;
        let refuse = |why: &str| {
            Err(std::io::Error::other(format!(
                "refusing output directory {}: {why}",
                dir.display()
            )))
        };
        if !meta.is_dir() {
            return refuse("not a directory (or a symlink)");
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            return refuse("owned by another user");
        }
        if meta.mode() & 0o022 != 0 {
            return refuse("writable by group or others");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// A scratch `HOME` for spawned binaries, so a scan never reads or writes the
/// developer's real profile and produces the same result on every machine.
pub fn isolated_home(cfg: &ScanConfig) -> std::path::PathBuf {
    isolated_home_ours(cfg)
}

pub fn isolated_home_ours(cfg: &ScanConfig) -> std::path::PathBuf {
    let home = cfg.raw_dir.join("isolated-home-ours");
    let _ = std::fs::create_dir_all(&home);
    home
}

pub fn isolated_home_upstream(cfg: &ScanConfig) -> std::path::PathBuf {
    let home = cfg.raw_dir.join("isolated-home-upstream");
    let _ = std::fs::create_dir_all(&home);
    let version_file = home
        .join(".local")
        .join("share")
        .join("inshellisense")
        .join("version.txt");
    let legacy_version = home.join(".inshellisense").join("version.txt");
    if !version_file.exists() && !legacy_version.exists() && cfg.upstream.exists() {
        let mut cmd = std::process::Command::new(&cfg.upstream);
        cmd.arg("reinit");
        isolate(&mut cmd, &home);
        let _ = cmd.output();
    }
    home
}

/// Environment variables we know change what a spawned binary does: spec
/// sources (`INSH_RS_SPECS_DIR`), feature toggles (`INSH_RS_NO_COREUTILS`),
/// upstream session markers (`ISTERM*`), and zsh startup redirection
/// (`ZDOTDIR`). Listed explicitly so they are stripped even when another
/// code path set them directly rather than inheriting them.
const MACHINE_SPECIFIC_VARS: &[&str] = &[
    "INSH_RS",
    "INSH_RS_LOGIN",
    "INSH_RS_TEST",
    "INSH_RS_SPECS_DIR",
    "INSH_RS_NO_COREUTILS",
    "ISTERM",
    "ISTERM_LOGIN",
    "ISTERM_TESTING",
    "ZDOTDIR",
];

/// Does this variable name change what a spawned binary does on this
/// machine? See [`MACHINE_SPECIFIC_VARS`].
pub fn is_machine_specific(name: &str) -> bool {
    // Our own namespace carries spec sources and feature toggles, so strip
    // every `INSH_RS*` export, not just the ones known today.
    name.starts_with("INSH_RS") || name.starts_with("ISTERM") || name == "ZDOTDIR"
}

/// Keys of the environment variables that would make a spawned binary
/// behave differently on this machine than on any other.
pub fn machine_specific_env_keys() -> Vec<std::ffi::OsString> {
    let mut keys: Vec<std::ffi::OsString> = MACHINE_SPECIFIC_VARS
        .iter()
        .map(std::ffi::OsStr::new)
        .map(std::ffi::OsString::from)
        .collect();
    keys.extend(
        std::env::vars_os()
            .map(|(k, _)| k)
            .filter(|k| is_machine_specific(&k.to_string_lossy())),
    );
    keys
}

/// Make a spawned binary see a machine-independent environment.
///
/// Every spawned binary must behave the same on every machine, and both
/// sides of a comparison must be configured identically by construction:
///
/// * Spec-source inputs — `INSH_RS_SPECS_DIR`, `[specs].path` from an
///   operator rc.toml, user TOML specs under the config dir — are cut off by
///   stripping every `INSH_RS*`/`ISTERM*`/`ZDOTDIR` variable and pointing
///   `HOME`, `USERPROFILE`, and `XDG_CONFIG_HOME` at the scan's scratch
///   home. Upstream honors none of them, so leaving them set compared our
///   configured side against upstream's unconfigured side.
/// * A host coreutils install would add specs to our side only, so probing
///   is disabled outright.
pub fn deterministic(cmd: &mut std::process::Command, home: &std::path::Path) {
    // Set last, so `INSH_RS_NO_COREUTILS` survives its own prefix strip.
    for key in machine_specific_env_keys() {
        cmd.env_remove(key);
    }
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("INSH_RS_NO_COREUTILS", "1");
}

/// Like [`deterministic`], plus a fixed working directory for children whose
/// output depends on cwd.
pub fn isolate(cmd: &mut std::process::Command, home: &std::path::Path) {
    deterministic(cmd, home);
    cmd.current_dir(home);
}

/// Can this binary actually be executed? Every category converts a spawn
/// error into placeholder text and then compares placeholders, so two broken
/// binaries used to score perfect parity. Establish up front that both sides
/// run at all.
pub fn spawn_check(bin: &std::path::Path) -> Result<(), String> {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("cannot execute {}: {e}", bin.display())),
    }
}

/// Drive all requested categories. Each category owns its own IO.
pub fn run_scan(cfg: &ScanConfig) -> Report {
    let mut report = Report::new();

    // Preflight: an unrunnable binary is a scanner failure, not 100% parity.
    let preflight: Vec<String> = [&cfg.ours, &cfg.upstream]
        .iter()
        .filter_map(|bin| spawn_check(bin).err())
        .collect();
    if !preflight.is_empty() {
        for cat in &cfg.categories {
            let mut cat_report = CategoryReport::new(*cat);
            for reason in &preflight {
                cat_report.push_fail("preflight", reason.clone(), Vec::new(), 3);
            }
            report.categories.push(cat_report);
        }
        return report;
    }

    for cat in &cfg.categories {
        if cfg.verbose {
            eprintln!("parity-scan: running {} …", cat.name());
        }
        let cat_report = match cat {
            Category::Cli => cli::run(cfg),
            Category::Init => init::run(cfg),
            Category::Doctor => doctor::run(cfg),
            Category::Complete => complete::run(cfg),
            Category::Specs => specs::run(cfg),
            #[cfg(unix)]
            Category::Render => render::run(cfg),
        };
        if cfg.verbose {
            eprintln!(
                "parity-scan: {} done — {}/{} pass ({:.1}%)",
                cat.name(),
                cat_report.pass_count(),
                cat_report.cases.len(),
                cat_report.score() * 100.0
            );
        }
        report.categories.push(cat_report);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The output directory holds captures and the scratch HOMEs whose
    /// startup files spawned shells source, so it must be private (#88).
    #[cfg(unix)]
    #[test]
    fn output_dir_is_created_private_and_shared_ones_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::test_support::unique_temp_dir("parity-out");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // Created fresh: 0700, and reusing it is fine.
        let fresh = base.join("a").join("raw");
        prepare_output_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        prepare_output_dir(&fresh).unwrap();

        // A directory others can write into is refused.
        let shared = base.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_output_dir(&shared).is_err());

        // So is a symlink standing in for the directory.
        let link = base.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(prepare_output_dir(&link).is_err());

        let _ = std::fs::remove_dir_all(base);
    }

    /// A category with no cases proved nothing. Scoring it 1.0 let a deleted
    /// or unreadable corpus report perfect parity and clear the threshold.
    #[test]
    fn empty_category_scores_zero() {
        let report = CategoryReport::new(Category::Complete);
        assert_eq!(report.score(), 0.0);
    }

    /// Likewise, an empty report (e.g. `--only` matched nothing) must not
    /// fold to a vacuous minimum of 1.0.
    #[test]
    fn empty_report_min_score_is_zero() {
        assert_eq!(Report::new().min_score(), 0.0);
    }

    #[test]
    fn min_score_picks_the_worst_category() {
        let mut report = Report::new();
        let mut good = CategoryReport::new(Category::Cli);
        good.push_pass("a");
        good.push_pass("b");
        let mut bad = CategoryReport::new(Category::Init);
        bad.push_pass("a");
        bad.push_fail("b", "diverged".to_string(), Vec::new(), 1);
        report.categories.push(good);
        report.categories.push(bad);
        assert_eq!(report.min_score(), 0.5);
    }

    /// Spec-source and session inputs must not reach the child. The poisoned
    /// values are set on the `Command` (not the parent process) so the test
    /// stays hermetic; `env_remove` must beat them.
    #[cfg(unix)]
    #[test]
    fn deterministic_strips_spec_sources_and_pins_home() {
        let home = crate::test_support::unique_temp_dir("parity-home");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(
            "for v in INSH_RS_SPECS_DIR ISTERM ZDOTDIR; do \
                 if printenv \"$v\" >/dev/null; then echo \"leaked $v\"; fi; done; \
                 printf 'coreutils=%s\\n' \"$INSH_RS_NO_COREUTILS\"; \
                 printf 'home=%s\\n' \"$HOME\"",
        );
        // What an operator export (or a previous category) might have left.
        cmd.env("INSH_RS_SPECS_DIR", "/tmp/operator-specs")
            .env("ISTERM", "1")
            .env("ZDOTDIR", "/tmp/operator-zsh")
            .env("INSH_RS_NO_COREUTILS", "0")
            .env("UNRELATED", "keep");
        deterministic(&mut cmd, &home);
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!text.contains("leaked"), "stripped vars leaked: {text}");
        assert!(text.contains("coreutils=1"), "{text}");
        assert!(
            text.contains(&format!("home={}", home.display())),
            "HOME not pinned: {text}"
        );
    }
}
