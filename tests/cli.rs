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
fn complete_attached_shell_flag_is_not_part_of_the_line() {
    let home = temp_dir("pwsh-attached-shell");
    let out = run_with_home(&home, &["complete", "-spwsh", "ls -"]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        json["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "-Recurse")
    );
}

#[test]
fn complete_delimiter_preserves_literal_shell_and_help_flags() {
    let home = temp_dir("complete-literal-flags");
    let out = run_with_home(
        &home,
        &[
            "complete",
            "--shell",
            "bash",
            "--",
            "--shell",
            "not-a-real-shell",
            "--help",
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["activeToken"]["token"], "--help");
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

/// A profile and the spawn command both source the PowerShell integration.
/// A second load must preserve the user's original Prompt (#116).
#[cfg(windows)]
#[test]
fn powershell_integration_can_be_sourced_repeatedly() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/powershell-integration.ps1");
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .output()
        .expect("Windows PowerShell is required on supported Windows versions");
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn fish_integration_preserves_prompts_and_provider_policy_on_repeated_loads() {
    let Some(fish) = inshellisense_rs::platform::find_on_path("fish") else {
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = temp_dir("fish-integration");
    let out = Command::new(fish)
        .arg("--no-config")
        .arg(root.join("tests/fish-integration.fish"))
        .arg(root.join("shell/shellIntegration.fish"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(home).unwrap();
}

#[cfg(unix)]
#[test]
fn wrapped_fish_selects_native_provider_from_effective_ui_without_changing_parent() {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    if inshellisense_rs::platform::find_on_path("fish").is_none() {
        return;
    }
    let parent_policy = std::env::var_os("INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS");
    for (configured, ui_override, inherited, expected) in [
        ("ghost", None, "0", "1:0"),
        ("hybrid", None, "0", "1:0"),
        ("popup", None, "1", "0:1"),
        ("ghost", Some("popup"), "1", "0:1"),
        ("popup", Some("hybrid"), "0", "1:0"),
    ] {
        let home = temp_dir("fish-ui-policy");
        let config = home.join(".config");
        std::fs::create_dir_all(config.join("fish")).unwrap();
        std::fs::create_dir_all(config.join("inshellisense-rs")).unwrap();
        std::fs::write(
            config.join("inshellisense-rs/rc.toml"),
            format!("ui = \"{configured}\"\n"),
        )
        .unwrap();
        std::fs::write(config.join("fish/config.fish"), r#"
function fish_prompt
    printf '%s:%s' "$INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS" "$fish_autosuggestion_enabled" > "$HOME/provider-policy"
    printf '> '
end
"#).unwrap();
        let mut command = Command::new(bin());
        command.args(["start", "--shell", "fish"]);
        if let Some(ui) = ui_override {
            command.args(["--ui", ui]);
        }
        let env: Vec<(String, String)> = vec![
            ("HOME".into(), home.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME".into(),
                config.to_string_lossy().into_owned(),
            ),
            (
                "INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS".into(),
                inherited.into(),
            ),
            ("fish_autosuggestion_enabled".into(), "1".into()),
            ("INSH_RS_NO_COREUTILS".into(), "1".into()),
            ("ISTERM".into(), "0".into()),
            ("INSH_RS".into(), "0".into()),
            ("ISTERM_TESTING".into(), "0".into()),
            ("INSH_RS_TEST".into(), "0".into()),
        ];
        // Command prepares the environment before fork, allowing this fixture
        // to run beside tests that are also spawning processes. Mutating the
        // Rust environment after fork can inherit a locked environment mutex.
        let mut master_fd = 0;
        let mut slave_fd = 0;
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
        unsafe {
            libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK);
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
            .envs(env)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let mut child = command.spawn().unwrap();
        let started = Instant::now();
        let mut output = Vec::new();
        let mut terminal_query = Vec::new();
        let mut observed = String::new();
        let mut exited = false;
        while started.elapsed() < Duration::from_secs(15) {
            let mut buf = [0; 4096];
            if let Ok(n) = master.read(&mut buf) {
                output.extend_from_slice(&buf[..n]);
                // Fish probes terminal capabilities before painting its first
                // prompt. Act as the outer terminal rather than waiting for
                // its ten-second unanswered-query timeout in every case.
                terminal_query.extend_from_slice(&buf[..n]);
                let mut answered_through = 0;
                for end in 3..=terminal_query.len() {
                    let prefix = &terminal_query[..end];
                    if prefix.ends_with(b"\x1b[c") || prefix.ends_with(b"\x1b[0c") {
                        master.write_all(b"\x1b[?1;2c").unwrap();
                        answered_through = end;
                    }
                }
                terminal_query.drain(..answered_through);
                if terminal_query.len() > 3 {
                    terminal_query.drain(..terminal_query.len() - 3);
                }
            }
            if let Ok(policy) = std::fs::read_to_string(home.join("provider-policy"))
                && policy.len() == 3
            {
                observed = policy;
                break;
            }
            if child.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
            let mut pollfd = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut pollfd, 1, 20);
            }
        }
        // Closing the outer terminal terminates the fixture wrapper/session,
        // including on a failed assertion. Reap it before reporting failures.
        drop(master);
        let reaping = Instant::now();
        while !exited && reaping.elapsed() < Duration::from_secs(5) {
            exited = child.try_wait().unwrap().is_some();
            if !exited {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if !exited {
            let _ = child.kill();
        }
        child.wait().unwrap();
        std::fs::remove_dir_all(home).unwrap();
        assert!(exited, "Fish wrapper fixture did not exit");
        assert_eq!(
            observed,
            expected,
            "configured={configured}, override={ui_override:?}, output={}",
            String::from_utf8_lossy(&output)
        );
    }
    assert_eq!(
        std::env::var_os("INSH_RS_DISABLE_FISH_AUTOSUGGESTIONS"),
        parent_policy
    );
}

#[cfg(unix)]
#[test]
fn init_repairs_resource_fifos_without_waiting_for_a_writer() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let home = temp_dir("resource-fifo");
    assert!(run_with_home(&home, &["init", "pwsh"]).status.success());
    for relative in ["shell/shellIntegration.ps1", "version.txt"] {
        let path = home.join(".inshellisense").join(relative);
        std::fs::remove_file(&path).unwrap();
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

        let mut child = isolated_cmd(&home)
            .args(["init", "pwsh"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "init failed for {relative}");
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("init blocked on {relative}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(std::fs::symlink_metadata(&path).unwrap().is_file());
        assert!(!std::fs::read(&path).unwrap().is_empty());
    }
    std::fs::remove_dir_all(home).unwrap();
}

/// No writer is needed for FIFO inputs: diagnose them, and leave profile
/// editing and resource removal untouched when the profile cannot be read.
#[cfg(unix)]
#[test]
fn fifo_configs_and_profiles_return_diagnostics_without_blocking_or_editing() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    let home = temp_dir("config-profile-fifo");
    let config = home.join(".inshellisenserc");
    let make_fifo = |path: &Path| {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    };
    make_fifo(&config);
    // Doctor reaches the configuration diagnostics without loading the spec
    // bundle, keeping this deadline specific to file I/O rather than CPU load.
    let mut command = isolated_cmd(&home);
    command.arg("doctor");
    let output = output_with_deadline(command);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(".inshellisenserc"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected a regular file"));
    std::fs::remove_file(config).unwrap();

    let rc = home.join(".bashrc");
    make_fifo(&rc);
    for args in [vec!["doctor"], vec!["init", "bash", "--install-rc"]] {
        let mut command = isolated_cmd(&home);
        command.args(&args);
        let output = output_with_deadline(command);
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("expected a regular file"),
            "{args:?}: {stderr}"
        );
        if args[0] == "init" {
            assert!(stderr.contains(".bashrc"), "{stderr}");
        }
        assert!(
            std::fs::symlink_metadata(&rc)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
    let preserved = home.join(".inshellisense/preserved");
    std::fs::write(&preserved, b"user resources").unwrap();
    let mut command = isolated_cmd(&home);
    command.arg("uninstall");
    let output = output_with_deadline(command);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&preserved).unwrap(), b"user resources");
    assert!(
        std::fs::symlink_metadata(&rc)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    std::fs::remove_dir_all(home).unwrap();
}

#[cfg(unix)]
fn output_with_deadline(mut command: Command) -> std::process::Output {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CLI blocked on a FIFO config or profile");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn ordinary_config_files_keep_their_precedence() {
    let home = temp_dir("config-precedence");
    let config_paths = [
        home.join(".inshellisenserc"),
        home.join(".config/inshellisense/rc.toml"),
        home.join(".config/inshellisense-rs/rc.toml"),
    ];
    for (index, config) in config_paths.iter().enumerate() {
        let specs = home.join(format!("specs-{index}"));
        std::fs::create_dir(&specs).unwrap();
        let spec = serde_json::json!({"names":["configaudit"], "args":[{"suggestions":[{"name":format!("winner-{index}")}]}]});
        std::fs::write(specs.join("configaudit.json"), spec.to_string()).unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let path = serde_json::to_string(&specs.to_string_lossy()).unwrap();
        std::fs::write(config, format!("[specs]\npath = [{path}]\n")).unwrap();
    }
    for index in (0..3).rev() {
        let output = isolated_cmd(&home)
            .env("APPDATA", home.join(".config"))
            .args(["complete", "configaudit "])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        let expected = format!("winner-{index}");
        assert!(
            json["suggestions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == expected),
            "{json}"
        );
        std::fs::remove_file(&config_paths[index]).unwrap();
    }
    std::fs::remove_dir_all(home).unwrap();
}
