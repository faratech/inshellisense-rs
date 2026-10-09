//! `parity-scan` — standalone tool that compares inshellisense-rs against
//! upstream inshellisense across six categories (cli, init, doctor,
//! complete, specs, render) and emits a markdown report.
//!
//! Usage:
//!     cargo run --release --bin parity-scan -- \
//!         --ours target/release/is \
//!         --upstream <bench-dir>/node_modules/@microsoft/inshellisense-linux-x64/inshellisense-linux-x64 \
//!         --corpus tests/parity \
//!         --report target/parity/parity-report.md
//!
//! `--upstream` is required: the scanner executes it, so it is never taken
//! from a default location in the shared temp directory. Output defaults to
//! `target/parity/` under the current directory.

use inshellisense_rs::parity::{self, Category, ScanConfig, report};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = ScanConfig {
        ours: PathBuf::from("target/release/is"),
        // Required (checked below). The scanner executes this binary, so it
        // must not default to a predictable path in the shared temp dir.
        upstream: PathBuf::new(),
        corpus_dir: PathBuf::from("tests/parity"),
        // The developer's own build tree rather than the shared `/tmp`.
        output_path: PathBuf::from("target/parity/parity-report.md"),
        raw_dir: PathBuf::from("target/parity/raw"),
        categories: Category::all().to_vec(),
        verbose: false,
        threshold: 0.90,
    };

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        // Every value-taking flag needs its operand. Indexing `args[i]` after
        // a bare `--ours` panicked with an out-of-bounds slice index.
        let mut value = || -> Result<&String, ExitCode> {
            i += 1;
            args.get(i).ok_or_else(|| {
                eprintln!("parity-scan: missing value for `{}`", arg);
                ExitCode::from(2)
            })
        };
        match arg.as_str() {
            "--ours" => match value() {
                Ok(v) => cfg.ours = PathBuf::from(v),
                Err(code) => return code,
            },
            "--upstream" => match value() {
                Ok(v) => cfg.upstream = PathBuf::from(v),
                Err(code) => return code,
            },
            "--corpus" => match value() {
                Ok(v) => cfg.corpus_dir = PathBuf::from(v),
                Err(code) => return code,
            },
            "--report" => match value() {
                Ok(v) => cfg.output_path = PathBuf::from(v),
                Err(code) => return code,
            },
            "--raw-dir" => match value() {
                Ok(v) => cfg.raw_dir = PathBuf::from(v),
                Err(code) => return code,
            },
            "--only" => {
                let raw = match value() {
                    Ok(v) => v.clone(),
                    Err(code) => return code,
                };
                // Silently dropping unknown names selected zero categories,
                // which then scored a vacuous 100% and exited 0.
                let mut selected = Vec::new();
                for name in raw.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    match Category::parse(name) {
                        Some(cat) => selected.push(cat),
                        None => {
                            eprintln!("parity-scan: unknown category `{}`", name);
                            return ExitCode::from(2);
                        }
                    }
                }
                if selected.is_empty() {
                    eprintln!("parity-scan: `--only` selected no categories");
                    return ExitCode::from(2);
                }
                cfg.categories = selected;
            }
            "--threshold" => {
                let raw = match value() {
                    Ok(v) => v.clone(),
                    Err(code) => return code,
                };
                // `"NaN".parse::<f64>()` succeeds, and `min < NaN` is always
                // false — a NaN threshold could never fail the run.
                match raw.parse::<f64>() {
                    Ok(t) if t.is_finite() && (0.0..=1.0).contains(&t) => cfg.threshold = t,
                    _ => {
                        eprintln!(
                            "parity-scan: --threshold must be a number in [0.0, 1.0], got `{}`",
                            raw
                        );
                        return ExitCode::from(2);
                    }
                }
            }
            "-v" | "--verbose" => {
                cfg.verbose = true;
            }
            "-h" | "--help" => {
                print_help();
                return ExitCode::from(0);
            }
            other => {
                eprintln!("parity-scan: unknown arg `{}`", other);
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    if cfg.upstream.as_os_str().is_empty() {
        eprintln!("parity-scan: --upstream <PATH> is required");
        return ExitCode::from(2);
    }
    if let Err(code) = check_executable("--ours", &cfg.ours) {
        return code;
    }
    if let Err(code) = check_executable("--upstream", &cfg.upstream) {
        return code;
    }

    // Canonicalize binary paths so subprocess-with-current-dir still
    // finds them regardless of how cwd is set inside each category.
    if let Ok(abs) = cfg.ours.canonicalize() {
        cfg.ours = abs;
    }
    if let Ok(abs) = cfg.upstream.canonicalize() {
        cfg.upstream = abs;
    }
    if let Ok(abs) = cfg.corpus_dir.canonicalize() {
        cfg.corpus_dir = abs;
    }
    if let Err(e) = parity::prepare_output_dir(&cfg.raw_dir) {
        eprintln!("parity-scan: {e}");
        return ExitCode::from(2);
    }

    let report = parity::run_scan(&cfg);

    // Write markdown report.
    if let Err(e) = report::write_markdown(&report, &cfg.output_path) {
        eprintln!(
            "parity-scan: failed to write {}: {}",
            cfg.output_path.display(),
            e
        );
        return ExitCode::from(3);
    }

    // Console summary.
    println!();
    println!("parity-scan summary:");
    println!("┌──────────┬────────┬──────┬──────┐");
    println!("│ Category │  Score │ PASS │ FAIL │");
    println!("├──────────┼────────┼──────┼──────┤");
    for cat in &report.categories {
        println!(
            "│ {:<8} │ {:>5.1}% │ {:>4} │ {:>4} │",
            cat.category.name(),
            cat.score() * 100.0,
            cat.pass_count(),
            cat.fail_count()
        );
    }
    println!("└──────────┴────────┴──────┴──────┘");
    println!("report: {}", cfg.output_path.display());
    println!("raw captures: {}", cfg.raw_dir.display());

    let min = report.min_score();
    if min < cfg.threshold {
        eprintln!(
            "parity-scan: min category score {:.1}% below threshold {:.1}%",
            min * 100.0,
            cfg.threshold * 100.0
        );
        return ExitCode::from(1);
    }

    ExitCode::from(0)
}

/// `Path::exists` was the only gate, so two non-executable files compared as
/// perfect parity: both spawns failed identically and every category saw two
/// empty results.
fn check_executable(flag: &str, path: &std::path::Path) -> Result<(), ExitCode> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => {
            eprintln!(
                "parity-scan: {flag} binary not usable: {} ({e})",
                path.display()
            );
            return Err(ExitCode::from(2));
        }
    };
    if !meta.is_file() {
        eprintln!("parity-scan: {flag} is not a file: {}", path.display());
        return Err(ExitCode::from(2));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            eprintln!(
                "parity-scan: {flag} binary is not executable: {}",
                path.display()
            );
            return Err(ExitCode::from(2));
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "parity-scan — compare inshellisense-rs against upstream inshellisense

Usage: parity-scan [OPTIONS]

Options:
  --ours <PATH>        Path to inshellisense-rs binary (default: target/release/is)
  --upstream <PATH>    Path to upstream binary (required)
  --corpus <DIR>       Corpus directory (default: tests/parity)
  --report <PATH>      Output markdown report (default: target/parity/parity-report.md)
  --raw-dir <DIR>      Where to dump raw PTY captures; must be private to you
                       (default: target/parity/raw)
  --only <LIST>        Comma-separated category list: cli,init,doctor,complete,specs,render
  --threshold <0.0-1.0>  Minimum per-category pass rate (default: 0.90)
  -v, --verbose        Verbose progress output
  -h, --help           Show this help"
    );
}
