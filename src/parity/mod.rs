//! Parity scanner — systematic divergence detection between insh-rs
//! and upstream inshellisense.
//!
//! See `/root/.claude/plans/whimsical-weaving-hanrahan.md` for the full
//! design. tl;dr: for each category (cli, init, doctor, complete,
//! specs, render), run the same corpus of inputs through both binaries,
//! normalize their outputs into a common shape, and emit a markdown
//! report that ranks divergences by user impact.

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

    pub fn score(&self) -> f64 {
        if self.cases.is_empty() {
            return 1.0;
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

    pub fn min_score(&self) -> f64 {
        self.categories
            .iter()
            .map(|c| c.score())
            .fold(1.0_f64, f64::min)
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

/// Drive all requested categories. Each category owns its own IO.
pub fn run_scan(cfg: &ScanConfig) -> Report {
    let mut report = Report::new();
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
