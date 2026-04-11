//! Hand-ported fallback specs for commands the extractor can't fully
//! extract yet (because upstream uses factory exports, top-level async
//! closures, or complex inline generators).
//!
//! As the extractor gets smarter this list shrinks. Docker, systemctl,
//! ssh, and several other essentials are now extractor-sourced and no
//! longer need curated fallbacks.

use crate::spec::model::*;

pub fn all() -> Vec<Subcommand> {
    vec![git_spec(), cargo_spec()]
}

fn sub_desc(name: &str, desc: &str) -> Subcommand {
    Subcommand {
        names: vec![name.to_string()],
        description: Some(desc.to_string()),
        ..Default::default()
    }
}

fn opt(names: &[&str], desc: &str) -> Opt {
    Opt {
        names: names.iter().map(|s| s.to_string()).collect(),
        description: Some(desc.to_string()),
        ..Default::default()
    }
}

fn script_arg(name: &str, script: &str, ttl: u64) -> Arg {
    Arg {
        name: Some(name.to_string()),
        generators: vec![Generator::Script {
            input: ScriptInput::Shell {
                script: script.to_string(),
            },
            split_on: Some("\n".to_string()),
            post_process: PostProcess::None {},
            timeout_ms: 5000,
            cache: Some(CacheSpec {
                strategy: CacheStrategy::MaxAge,
                ttl_secs: ttl,
                by_dir: true,
            }),
        }],
        ..Default::default()
    }
}

fn branch_arg() -> Arg {
    script_arg(
        "branch",
        "git for-each-ref --format='%(refname:short)' refs/heads refs/remotes",
        10,
    )
}

fn git_spec() -> Subcommand {
    Subcommand {
        names: vec!["git".to_string()],
        description: Some("Distributed version control".to_string()),
        subcommands: vec![
            Subcommand {
                names: vec!["checkout".to_string()],
                description: Some("Switch branches or restore working tree files".to_string()),
                options: vec![
                    opt(&["-b"], "Create and switch to a new branch"),
                    opt(&["-B"], "Create/reset and switch to a branch"),
                    opt(&["--force", "-f"], "Force checkout"),
                ],
                args: vec![branch_arg()],
                ..Default::default()
            },
            Subcommand {
                names: vec!["switch".to_string()],
                description: Some("Switch branches".to_string()),
                options: vec![opt(&["-c"], "Create new branch")],
                args: vec![branch_arg()],
                ..Default::default()
            },
            Subcommand {
                names: vec!["branch".to_string()],
                description: Some("List/create/delete branches".to_string()),
                options: vec![
                    opt(&["-d", "--delete"], "Delete a branch"),
                    opt(&["-D"], "Force delete"),
                    opt(&["-a", "--all"], "List remote-tracking and local"),
                ],
                args: vec![branch_arg()],
                ..Default::default()
            },
            sub_desc("status", "Show working tree status"),
            sub_desc("log", "Show commit logs"),
            sub_desc("diff", "Show changes"),
            sub_desc("fetch", "Download objects and refs"),
            sub_desc("pull", "Fetch + merge"),
            sub_desc("push", "Update remote refs"),
            sub_desc("stash", "Stash changes"),
            sub_desc("rebase", "Reapply commits"),
            sub_desc("merge", "Merge branches"),
            sub_desc("commit", "Record changes"),
            sub_desc("add", "Add file contents to the index"),
            sub_desc("reset", "Reset HEAD"),
            sub_desc("clone", "Clone a repository"),
            sub_desc("tag", "Manage tags"),
            sub_desc("remote", "Manage set of tracked repositories"),
            sub_desc("show", "Show various types of objects"),
            sub_desc("restore", "Restore working tree files"),
            sub_desc("cherry-pick", "Apply commits from other branches"),
            sub_desc("worktree", "Manage worktrees"),
            sub_desc("config", "Get and set options"),
            sub_desc("init", "Create an empty repo"),
            sub_desc("revert", "Revert commits"),
            sub_desc("bisect", "Binary search for regressions"),
            sub_desc("blame", "Show what revision introduced each line"),
            sub_desc("reflog", "Manage reflog information"),
        ],
        options: vec![
            Opt {
                names: vec!["-C".to_string()],
                description: Some("Run as if git was started in <path>".to_string()),
                args: vec![Arg {
                    name: Some("path".to_string()),
                    templates: vec![Template::Folders],
                    ..Default::default()
                }],
                is_persistent: true,
                ..Default::default()
            },
            opt(&["--version"], "Print version"),
            opt(&["--help", "-h"], "Show help"),
        ],
        ..Default::default()
    }
}

fn cargo_spec() -> Subcommand {
    Subcommand {
        names: vec!["cargo".to_string()],
        description: Some("Rust package manager".to_string()),
        subcommands: vec![
            sub_desc("build", "Compile the current package"),
            sub_desc("run", "Run a binary"),
            sub_desc("test", "Run tests"),
            sub_desc("check", "Check a package"),
            sub_desc("clippy", "Lint"),
            sub_desc("fmt", "Format code"),
            sub_desc("add", "Add a dependency"),
            sub_desc("remove", "Remove a dependency"),
            sub_desc("update", "Update deps"),
            sub_desc("publish", "Publish crate"),
            sub_desc("install", "Install a binary"),
            sub_desc("new", "Create a new package"),
            sub_desc("init", "Init a package"),
            sub_desc("doc", "Build documentation"),
            sub_desc("bench", "Benchmark"),
            sub_desc("tree", "Dependency tree"),
        ],
        options: vec![
            opt(&["--release"], "Build with release profile"),
            opt(&["--features"], "Enable features"),
            opt(&["--all-features"], "Enable all features"),
            opt(&["--no-default-features"], "Disable default features"),
        ],
        ..Default::default()
    }
}
