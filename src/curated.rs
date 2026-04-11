//! Hand-ported Fig specs for smoke tests during phase 1/2.
//!
//! Will be replaced by the extractor's msgpack output in phase 3. For now
//! we keep just enough to exercise the v2 schema (subcommand resolution,
//! option-value binding, generator execution).

use crate::spec::model::*;

pub fn all() -> Vec<Subcommand> {
    vec![git_spec(), docker_spec(), cargo_spec(), systemctl_spec(), ssh_spec()]
}

fn sub(name: &str) -> Subcommand {
    Subcommand::new(name)
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
            input: ScriptInput::Shell { script: script.to_string() },
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
        ],
        options: vec![
            // -C takes a path argument — correctness test for option-value binding.
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

fn docker_spec() -> Subcommand {
    Subcommand {
        names: vec!["docker".to_string()],
        description: Some("Container runtime".to_string()),
        subcommands: vec![
            Subcommand {
                names: vec!["run".to_string()],
                description: Some("Run a command in a new container".to_string()),
                options: vec![
                    opt(&["-i", "--interactive"], "Keep STDIN open"),
                    opt(&["-t", "--tty"], "Allocate a pseudo-TTY"),
                    opt(&["-d", "--detach"], "Detached mode"),
                    opt(&["--rm"], "Remove container on exit"),
                    opt(&["-p", "--publish"], "Publish port"),
                    opt(&["-v", "--volume"], "Mount volume"),
                    opt(&["--name"], "Container name"),
                ],
                ..Default::default()
            },
            Subcommand {
                names: vec!["exec".to_string()],
                description: Some("Run a command in a running container".to_string()),
                options: vec![opt(&["-it"], "Interactive tty")],
                args: vec![script_arg(
                    "container",
                    "docker ps --format '{{.Names}}'",
                    3,
                )],
                ..Default::default()
            },
            sub_desc("ps", "List containers"),
            sub_desc("images", "List images"),
            sub_desc("build", "Build an image"),
            sub_desc("pull", "Pull an image"),
            sub_desc("push", "Push an image"),
            sub_desc("rm", "Remove containers"),
            sub_desc("rmi", "Remove images"),
            sub_desc("logs", "Fetch container logs"),
            sub_desc("stop", "Stop containers"),
            sub_desc("start", "Start containers"),
            sub_desc("restart", "Restart containers"),
            sub_desc("compose", "Docker Compose"),
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

fn systemctl_spec() -> Subcommand {
    let unit_arg = || script_arg(
        "unit",
        "systemctl list-unit-files --no-legend --no-pager | awk '{print $1}'",
        60,
    );
    Subcommand {
        names: vec!["systemctl".to_string()],
        description: Some("systemd control".to_string()),
        subcommands: vec![
            Subcommand { args: vec![unit_arg()], ..sub_desc("start", "Start units") },
            Subcommand { args: vec![unit_arg()], ..sub_desc("stop", "Stop units") },
            Subcommand { args: vec![unit_arg()], ..sub_desc("restart", "Restart units") },
            Subcommand { args: vec![unit_arg()], ..sub_desc("status", "Show status") },
            Subcommand { args: vec![unit_arg()], ..sub_desc("enable", "Enable") },
            Subcommand { args: vec![unit_arg()], ..sub_desc("disable", "Disable") },
            sub_desc("daemon-reload", "Reload systemd"),
            sub_desc("list-units", "List loaded units"),
        ],
        options: vec![opt(&["--user"], "User instance")],
        ..Default::default()
    }
}

fn ssh_spec() -> Subcommand {
    Subcommand {
        names: vec!["ssh".to_string()],
        description: Some("OpenSSH client".to_string()),
        options: vec![
            opt(&["-p"], "Port"),
            opt(&["-i"], "Identity file"),
            opt(&["-v"], "Verbose"),
            opt(&["-A"], "Forward auth agent"),
            opt(&["-X"], "X11 forwarding"),
            opt(&["-N"], "Do not execute remote command"),
            opt(&["-f"], "Go to background"),
        ],
        args: vec![script_arg(
            "host",
            "awk '/^Host [^*]/ {for (i=2; i<=NF; i++) print $i}' ~/.ssh/config 2>/dev/null",
            300,
        )],
        ..Default::default()
    }
}
