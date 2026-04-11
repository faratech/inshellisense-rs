//! `parity-scan` — standalone tool that compares insh-rs against
//! upstream inshellisense across six categories (cli, init, doctor,
//! complete, specs, render) and emits a markdown report.
//!
//! Usage:
//!     cargo run --release --bin parity-scan -- \
//!         --ours target/release/insh \
//!         --upstream /tmp/insh-bench/node_modules/@microsoft/inshellisense-linux-x64/inshellisense-linux-x64 \
//!         --corpus tests/parity \
//!         --report /tmp/parity-report.md

use insh_rs::parity::{self, report, Category, ScanConfig};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = ScanConfig {
        ours: PathBuf::from("target/release/insh"),
        upstream: PathBuf::from(
            "/tmp/insh-bench/node_modules/@microsoft/inshellisense-linux-x64/inshellisense-linux-x64",
        ),
        corpus_dir: PathBuf::from("tests/parity"),
        output_path: PathBuf::from("/tmp/parity-report.md"),
        raw_dir: PathBuf::from("/tmp/parity-out"),
        categories: Category::all().to_vec(),
        verbose: false,
        threshold: 0.90,
    };

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--ours" => {
                i += 1;
                cfg.ours = PathBuf::from(&args[i]);
            }
            "--upstream" => {
                i += 1;
                cfg.upstream = PathBuf::from(&args[i]);
            }
            "--corpus" => {
                i += 1;
                cfg.corpus_dir = PathBuf::from(&args[i]);
            }
            "--report" => {
                i += 1;
                cfg.output_path = PathBuf::from(&args[i]);
            }
            "--raw-dir" => {
                i += 1;
                cfg.raw_dir = PathBuf::from(&args[i]);
            }
            "--only" => {
                i += 1;
                cfg.categories = args[i]
                    .split(',')
                    .filter_map(Category::parse)
                    .collect();
            }
            "--threshold" => {
                i += 1;
                cfg.threshold = args[i].parse().unwrap_or(0.90);
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

    if !cfg.ours.exists() {
        eprintln!("parity-scan: --ours binary not found: {}", cfg.ours.display());
        return ExitCode::from(2);
    }
    if !cfg.upstream.exists() {
        eprintln!(
            "parity-scan: --upstream binary not found: {}",
            cfg.upstream.display()
        );
        return ExitCode::from(2);
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

fn print_help() {
    println!(
        "parity-scan — compare insh-rs against upstream inshellisense

Usage: parity-scan [OPTIONS]

Options:
  --ours <PATH>        Path to insh-rs binary (default: target/release/insh)
  --upstream <PATH>    Path to upstream binary
  --corpus <DIR>       Corpus directory (default: tests/parity)
  --report <PATH>      Output markdown report (default: /tmp/parity-report.md)
  --raw-dir <DIR>      Where to dump raw PTY captures (default: /tmp/parity-out)
  --only <LIST>        Comma-separated category list: cli,init,doctor,complete,specs,render
  --threshold <0.0-1.0>  Minimum per-category pass rate (default: 0.90)
  -v, --verbose        Verbose progress output
  -h, --help           Show this help"
    );
}
