//! Dual-guard env var helpers.
//!
//! inshellisense-rs coexists with Microsoft's upstream inshellisense: we read BOTH
//! `INSH_RS` and `ISTERM` for session detection, and we SET both whenever
//! we spawn a wrapped shell. Same for the `_LOGIN` and `_TESTING` suffixes.
//! This makes our binary drop-in compatible with upstream shell integrations
//! and lets users migrate either direction.

/// Is a marker variable set to a value that actually means "on"?
///
/// `env::var(..).is_ok()` is true for `ISTERM=` and `ISTERM=0`, so an
/// explicitly cleared or disabled marker used to read as an active session —
/// making `ISTERM=0 is start` trip the re-entry guard and refuse to run.
/// The integrations only ever set these to `1`.
fn flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// Check if we're currently inside a wrapped shell session.
pub fn session_active() -> bool {
    flag_enabled("ISTERM") || flag_enabled("INSH_RS")
}

/// Whether the current invocation was spawned as a login shell.
pub fn login_active() -> bool {
    flag_enabled("ISTERM_LOGIN") || flag_enabled("INSH_RS_LOGIN")
}

/// Whether we're in test mode (deterministic prompt, no user config).
pub fn test_active() -> bool {
    flag_enabled("ISTERM_TESTING") || flag_enabled("INSH_RS_TEST")
}

/// Base env vars to set when spawning a wrapped shell.
pub fn spawn_env(login: bool, test: bool) -> Vec<(&'static str, &'static str)> {
    let mut out = vec![("ISTERM", "1"), ("INSH_RS", "1")];
    if login {
        out.push(("ISTERM_LOGIN", "1"));
        out.push(("INSH_RS_LOGIN", "1"));
    }
    if test {
        out.push(("ISTERM_TESTING", "1"));
        out.push(("INSH_RS_TEST", "1"));
    }
    out
}

/// Skip probing for an installed coreutils multi-call binary.
pub fn coreutils_disabled() -> bool {
    flag_enabled("INSH_RS_NO_COREUTILS")
}

/// VS Code sets this during environment resolution; shell integrations must
/// skip wrapping in this case to avoid interfering with VS Code's env probe.
pub fn vscode_resolving() -> bool {
    flag_enabled("VSCODE_RESOLVING_ENVIRONMENT")
}

#[cfg(test)]
mod tests {
    use super::flag_enabled;

    /// `set_var` mutates process-global state, so these run under one lock and
    /// clean up after themselves rather than racing sibling tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_var(name: &str, value: Option<&str>, check: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK; no other thread reads these names.
        unsafe {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        check();
        unsafe { std::env::remove_var(name) };
    }

    #[test]
    fn unset_marker_is_disabled() {
        with_var("INSH_RS_FLAG_TEST", None, || {
            assert!(!flag_enabled("INSH_RS_FLAG_TEST"))
        });
    }

    #[test]
    fn empty_and_zero_markers_are_disabled() {
        for value in ["", "0", "  ", "false", "FALSE"] {
            with_var("INSH_RS_FLAG_TEST_OFF", Some(value), || {
                assert!(
                    !flag_enabled("INSH_RS_FLAG_TEST_OFF"),
                    "{value:?} should not enable the flag"
                )
            });
        }
    }

    #[test]
    fn one_enables_marker() {
        for value in ["1", "true", "yes"] {
            with_var("INSH_RS_FLAG_TEST_ON", Some(value), || {
                assert!(
                    flag_enabled("INSH_RS_FLAG_TEST_ON"),
                    "{value:?} should enable the flag"
                )
            });
        }
    }
}
