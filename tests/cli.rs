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

fn run_with_home(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .output()
        .unwrap()
}

#[test]
fn complete_cwd_flag_before_line_uses_cwd_not_line() {
    let dir = temp_dir("complete-cwd");
    std::fs::write(dir.join("needle.txt"), "").unwrap();

    let out = Command::new(bin())
        .args(["complete", "--cwd", dir.to_str().unwrap(), "ls ne"])
        .output()
        .unwrap();
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
    let out = Command::new(bin())
        .args(["complete", "--text", "notarealcmd h"])
        .env("HOME", &home)
        .env("HISTFILE", &hist)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn complete_shell_pwsh_uses_powershell_ls_options() {
    let out = Command::new(bin())
        .args(["--shell", "pwsh", "complete", "ls -"])
        .output()
        .unwrap();
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
