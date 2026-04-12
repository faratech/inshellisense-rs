//! Dual-guard env var helpers.
//!
//! inshellisense-rs coexists with Microsoft's upstream inshellisense: we read BOTH
//! `INSH_RS` and `ISTERM` for session detection, and we SET both whenever
//! we spawn a wrapped shell. Same for the `_LOGIN` and `_TESTING` suffixes.
//! This makes our binary drop-in compatible with upstream shell integrations
//! and lets users migrate either direction.

/// Check if we're currently inside a wrapped shell session.
pub fn session_active() -> bool {
    std::env::var("ISTERM").is_ok() || std::env::var("INSH_RS").is_ok()
}

/// Whether the current invocation was spawned as a login shell.
pub fn login_active() -> bool {
    std::env::var("ISTERM_LOGIN").is_ok() || std::env::var("INSH_RS_LOGIN").is_ok()
}

/// Whether we're in test mode (deterministic prompt, no user config).
pub fn test_active() -> bool {
    std::env::var("ISTERM_TESTING").is_ok() || std::env::var("INSH_RS_TEST").is_ok()
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

/// VS Code sets this during environment resolution; shell integrations must
/// skip wrapping in this case to avoid interfering with VS Code's env probe.
pub fn vscode_resolving() -> bool {
    std::env::var("VSCODE_RESOLVING_ENVIRONMENT").is_ok()
}
