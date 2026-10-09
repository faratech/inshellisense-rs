use inshellisense_rs::parity;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_is")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "inshellisense-rs-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `Command` pointing at the binary with a scrubbed environment.
///
/// The completion tests used to inherit whatever the invoking shell had
/// exported: a developer's `INSH_RS_SPECS_DIR`, an rc.toml `[specs].path`,
/// user TOML specs under their config dir, or a host coreutils install
/// could each flip an assertion (or make one pass only on one machine).
/// [`parity::deterministic`] is the same machine-independent contract the
/// parity scanner imposes on its own children.
fn isolated_cmd(home: &Path) -> Command {
    let mut cmd = Command::new(bin());
    parity::deterministic(&mut cmd, home);
    cmd
}

fn run_with_home(home: &Path, args: &[&str]) -> std::process::Output {
    isolated_cmd(home).args(args).output().unwrap()
}

#[test]
fn complete_cwd_flag_before_line_uses_cwd_not_line() {
    let dir = temp_dir("complete-cwd");
    std::fs::write(dir.join("needle.txt"), "").unwrap();
    let home = temp_dir("complete-cwd-home");

    let out = run_with_home(
        &home,
        &["complete", "--cwd", dir.to_str().unwrap(), "ls ne"],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = json["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(names.contains(&"needle.txt"), "names={names:?}");
}

#[test]
fn init_install_rc_targets_requested_shell() {
    let home = temp_dir("init-zsh");
    let out = run_with_home(&home, &["init", "zsh", "--install-rc"]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let zshrc = std::fs::read_to_string(home.join(".zshrc")).unwrap();
    assert!(zshrc.contains("init/zsh/init.zsh"));
    assert!(!home.join(".bashrc").exists());
}

/// The harness must scrub host state: a spec dir exported by the
/// developer (the documented way to iterate on local specs) used to leak
/// into every spawned child and could flip assertions.
#[test]
fn operator_spec_dir_env_does_not_leak_into_children() {
    let specs_dir = temp_dir("env-scrub-private-specs");
    std::fs::write(
        specs_dir.join("zzzprivateaudit.json"),
        r#"{"names":["zzzprivateaudit"],"description":"must not be loaded"}"#,
    )
    .unwrap();
    let home = temp_dir("env-scrub-home");

    // What an inherited operator export looks like, applied to the command
    // exactly as the process environment would hand it down. The harness
    // scrub must win over it (`env_remove` after `env`), so the spec dir
    // never reaches the child.
    let mut cmd = Command::new(bin());
    cmd.env("INSH_RS_SPECS_DIR", &specs_dir);
    parity::deterministic(&mut cmd, &home);
    let out = cmd.args(["specs", "list", "--plain"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.lines().any(|l| l == "zzzprivateaudit"),
        "operator spec leaked into the child: {stdout}"
    );
    // Sanity check that the listing is real, not an empty failure.
    assert!(stdout.contains("git"), "stdout={stdout}");
}

#[test]
fn specs_path_from_config_loads_json_specs() {
    let home = temp_dir("specs-path");
    let config_dir = home.join(".config").join("inshellisense-rs");
    let specs_dir = home.join("json-specs");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&specs_dir).unwrap();
    std::fs::write(
        config_dir.join("rc.toml"),
        format!("[specs]\npath = [\"{}\"]\n", specs_dir.display()),
    )
    .unwrap();
    std::fs::write(
        specs_dir.join("helloaudit.json"),
        r#"{"names":["helloaudit"],"description":"audit spec"}"#,
    )
    .unwrap();

    let out = run_with_home(&home, &["specs", "list", "--plain"]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.lines().any(|l| l == "helloaudit"), "stdout={stdout}");
}

#[test]
fn uninstall_removes_bash_install_block() {
    let home = temp_dir("uninstall");
    let install = run_with_home(&home, &["install"]);
    assert!(
        install.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&install.stderr)
    );
    let before = std::fs::read_to_string(home.join(".bashrc")).unwrap();
    assert!(before.contains("# >>> inshellisense-rs init >>>"));

    let uninstall = run_with_home(&home, &["uninstall"]);
    assert!(
        uninstall.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&uninstall.stderr)
    );
    let after = std::fs::read_to_string(home.join(".bashrc")).unwrap_or_default();
    assert!(!after.contains("# >>> inshellisense-rs init >>>"));
}

#[test]
fn offline_text_complete_does_not_use_history() {
    let home = temp_dir("offline-history");
    let hist = home.join("history");
    std::fs::write(&hist, "notarealcmd hello world\n").unwrap();
    // `HISTFILE` is set after the scrub on purpose: it is this test's
    // input, not host state to isolate away.
    let out = isolated_cmd(&home)
        .env("HISTFILE", &hist)
        .args(["complete", "--text", "notarealcmd h"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

/// `complete --text` prints the ghost tail. A filename carrying an escape
/// sequence must come out with its control characters made visible, not
/// executed by the terminal (#85).
#[cfg(unix)]
#[test]
fn text_complete_never_prints_control_characters() {
    let dir = temp_dir("complete-text-ctl");
    std::fs::write(dir.join("evil\x1b]0;pwned\x07.txt"), "").unwrap();
    let home = temp_dir("complete-text-ctl-home");
    let out = run_with_home(
        &home,
        &[
            "complete",
            "--text",
            "--cwd",
            dir.to_str().unwrap(),
            "cat evi",
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains('\x1b'), "ESC reached stdout: {stdout:?}");
    assert!(!stdout.contains('\x07'), "BEL reached stdout: {stdout:?}");
    assert_eq!(stdout, "l?]0;pwned?.txt\n");
}

#[test]
fn complete_shell_pwsh_uses_powershell_ls_options() {
    let home = temp_dir("pwsh-ls");
    let out = run_with_home(&home, &["--shell", "pwsh", "complete", "ls -"]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = json["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(names.contains(&"-Recurse"), "names={names:?}");
    assert!(!names.contains(&"-l"), "names={names:?}");
}

/// parity-scan executes its `--upstream` binary, so it must be given one
/// explicitly rather than defaulting to a path under the shared `/tmp` (#88).
#[test]
fn parity_scan_requires_an_explicit_upstream() {
    let out = Command::new(env!("CARGO_BIN_EXE_parity-scan"))
        .args(["--ours", bin()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--upstream <PATH> is required"),
        "stderr={stderr}"
    );
}
