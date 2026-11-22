//! Execution of Fig-spec generators.
//!
//! Handles `Generator::Script`, `Generator::Template`, and `Generator::Glob`.
//! `Custom(FnId)` and `PostProcess::Fn` fall through to empty until phase 6
//! wires the js_bridge. Results are cached per (cwd, script) with TTL.
//!
//! Phase 2 change: multiple generators per arg execute concurrently on
//! scoped threads. One-or-zero generator still takes the fast path.

use crate::shell::Shell;
use crate::spec::model::{
    Arg, CacheSpec, CacheStrategy, Generator, PostProcess, PostProcessKind, ProjectFileReader,
    ScriptInput, Subcommand, Suggestion, SuggestionType, Template,
};
use crate::spec::parser::CommandToken;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

thread_local! {
    static REQUEST: std::cell::RefCell<Option<Cancellation>> = const { std::cell::RefCell::new(None) };
}

fn current_request() -> Option<Cancellation> {
    REQUEST.with(|slot| slot.borrow().clone())
}

/// Keep cancellation scoped to one completion, including its generator fan-out.
pub(crate) fn with_request<T>(request: Option<Cancellation>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Cancellation>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUEST.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(REQUEST.with(|slot| slot.replace(request)));
    run()
}

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Successful outputs are shared between the cached snapshot and any callers
/// awaiting the same execution. `None` means the subprocess failed.
type ScriptOutput = Option<Arc<Vec<String>>>;

#[derive(Default)]
struct CacheEntry {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    cached: Option<CachedOutput>,
    // A timeout governs execution, not the cached output. Calls with different
    // budgets share successful snapshots but must not inherit another run's
    // shorter deadline or extend a shorter caller's deadline.
    in_flight: HashMap<u32, Arc<CacheFlight>>,
}

struct CachedOutput {
    at: Instant,
    values: Arc<Vec<String>>,
}

#[derive(Default)]
struct CacheFlight {
    // Outer None: still running. Inner None: completed unsuccessfully.
    result: Mutex<Option<ScriptOutput>>,
    done: Condvar,
    cancel: Cancellation,
    waiters: AtomicUsize,
    refresh: bool,
}

impl CacheFlight {
    fn wait(&self, timeout_ms: u32, request: Option<&Cancellation>) -> ScriptOutput {
        let mut result = self.result.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms) + 50);
        while result.is_none() {
            if request.is_some_and(Cancellation::is_cancelled) {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let wait = if request.is_some() {
                remaining.min(POLL_INTERVAL)
            } else {
                remaining
            };
            (result, _) = self
                .done
                .wait_timeout(result, wait)
                .unwrap_or_else(|e| e.into_inner());
        }
        if request.is_some_and(Cancellation::is_cancelled) {
            return None;
        }
        result.as_ref().cloned().flatten()
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    command: Invocation,
    cwd: Option<String>,
    separator: String,
}

const MAX_CACHE_ENTRIES: usize = 512;

static CACHE: LazyLock<Mutex<HashMap<CacheKey, Arc<CacheEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `help_subcommands` are the subcommands of the command this arg belongs to.
/// The `help` template completes them (`git help <TAB>` → `commit`, `log`, …);
/// 93 bundled specs use it.
///
/// `shell` is the shell being wrapped — not necessarily the one that spawned
/// us. The `history` template reads the history file of THIS shell.
pub fn suggestions_for_arg(
    arg: &Arg,
    cwd: &str,
    prefix: &str,
    command_args: &[CommandToken],
    include_history: bool,
    help_subcommands: &[Subcommand],
    shell: Option<Shell>,
) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = arg.suggestions.clone();

    // Templates listed in the `templates` field do NOT add a `..` entry
    // (upstream `ls `/`vim ` omit it); the generator form does (see below).
    for tpl in &arg.templates {
        out.extend(template_suggestions(
            *tpl,
            cwd,
            prefix,
            include_history,
            false,
            help_subcommands,
            shell,
        ));
    }

    // Multi-generator fan-out: scoped threads run shell generators in
    // parallel. Single-generator arg stays on the caller's thread.
    match arg.generators.len() {
        0 => {}
        1 => {
            out.extend(run_generator(
                &arg.generators[0],
                cwd,
                prefix,
                command_args,
                include_history,
                help_subcommands,
                shell,
            ));
        }
        _ => {
            let request = current_request();
            let results: Vec<Vec<Suggestion>> = std::thread::scope(|scope| {
                let handles: Vec<_> = arg
                    .generators
                    .iter()
                    .map(|g| {
                        let request = request.clone();
                        scope.spawn(move || {
                            with_request(request, || {
                                run_generator(
                                    g,
                                    cwd,
                                    prefix,
                                    command_args,
                                    include_history,
                                    help_subcommands,
                                    shell,
                                )
                            })
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            for r in results {
                out.extend(r);
            }
        }
    }

    out
}

fn run_generator(
    g: &Generator,
    cwd: &str,
    prefix: &str,
    command_args: &[CommandToken],
    include_history: bool,
    help_subcommands: &[Subcommand],
    shell: Option<Shell>,
) -> Vec<Suggestion> {
    if current_request()
        .as_ref()
        .is_some_and(Cancellation::is_cancelled)
    {
        return Vec::new();
    }
    let mut out = run_generator_inner(
        g,
        cwd,
        prefix,
        command_args,
        include_history,
        help_subcommands,
        shell,
    );
    // Script output, glob matches and project files are whatever the
    // directory or the tools in it say, not spec text: mark them so the
    // accept path quotes them. Templates are handled by type (filesystem
    // names are File/Folder; `history` is the user's own command text and
    // `help` lists spec subcommands), and `FileExistsThen` yields the spec's
    // own subcommand names.
    if matches!(
        g,
        Generator::Script { .. }
            | Generator::Glob { .. }
            | Generator::ProjectFile { .. }
            | Generator::Zoxide
            | Generator::DockerImages
            | Generator::NpmRegistry
            | Generator::NpmWorkspaces
            | Generator::LocalLoginNames
            | Generator::NodeExecutables
            | Generator::RustToolchains
    ) {
        for s in &mut out {
            s.external = true;
        }
    }
    out
}

fn run_generator_inner(
    g: &Generator,
    cwd: &str,
    prefix: &str,
    command_args: &[CommandToken],
    include_history: bool,
    help_subcommands: &[Subcommand],
    shell: Option<Shell>,
) -> Vec<Suggestion> {
    match g {
        Generator::Script {
            input,
            split_on,
            post_process,
            timeout_ms,
            cache,
        } => run_script(
            input,
            split_on.as_deref(),
            post_process,
            *timeout_ms,
            cache.as_ref(),
            cwd,
            prefix,
        ),
        Generator::Template { template } => {
            // The generator form of a filepaths/folders template appends `..`
            // (upstream `python `/`node `/`cd ` show it).
            template_suggestions(
                *template,
                cwd,
                prefix,
                include_history,
                true,
                help_subcommands,
                shell,
            )
        }
        Generator::Glob { pattern } => glob_paths(pattern, cwd),
        Generator::Custom { .. } => Vec::new(), // always empty without JS
        Generator::ProjectFile { reader } => project_file_suggestions(*reader, cwd),
        Generator::Zoxide => zoxide_suggestions(cwd, command_args),
        Generator::DockerImages => docker_images(cwd),
        Generator::NpmRegistry => npm_registry(cwd, prefix),
        Generator::NpmWorkspaces => npm_workspaces(cwd),
        Generator::LocalLoginNames => local_login_names(),
        Generator::NodeExecutables => node_modules_binaries(cwd, false),
        Generator::RustToolchains => rust_toolchains(cwd, prefix),
        Generator::FileExistsThen {
            path,
            content_contains,
            subcommand,
        } => file_exists_then(path, content_contains.as_deref(), subcommand, cwd),
    }
}

fn zoxide_suggestions(cwd: &str, tokens: &[CommandToken]) -> Vec<Suggestion> {
    let mut argv = ["zoxide", "query", "--list", "--score"]
        .map(String::from)
        .to_vec();
    let mut end_of_options = false;
    let terms: Vec<_> = tokens
        .iter()
        .filter_map(|token| {
            if !end_of_options && token.is_option && token.token == "--" {
                end_of_options = true;
                return None;
            }
            if token.token.is_empty() || (!end_of_options && token.is_option) {
                return None;
            }
            Some(token.token.as_str())
        })
        .collect();
    if !terms.is_empty() {
        argv.extend(["--".into(), terms.join(" ")]);
    }

    let local = std::path::absolute(if cwd.is_empty() { "." } else { cwd }).ok();
    let raw = try_run_command(&Invocation::Argv(argv), cwd, 5_000);
    let legacy = raw.as_ref().is_err_and(|err| {
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
        )
    });
    let mut out = Vec::new();
    if let Ok(Some(raw)) = raw {
        for line in String::from_utf8_lossy(&raw).lines() {
            let Some((score, path)) = line.trim_start().split_once(char::is_whitespace) else {
                continue;
            };
            let Ok(score) = score.parse::<f64>() else {
                continue;
            };
            let path = path.trim_start();
            let absolute = Path::new(path);
            if !score.is_finite() || !absolute.is_absolute() {
                continue;
            }
            let immediate_child = local
                .as_deref()
                .is_some_and(|cwd| absolute.parent() == Some(cwd));
            let name = if immediate_child {
                let Some(name) = absolute.file_name() else {
                    continue;
                };
                name.to_string_lossy().into_owned()
            } else {
                path.to_owned()
            };
            out.push(Suggestion {
                name,
                description: Some(format!("Score: {score}")),
                icon: Some("💾".into()),
                // Local matches precede any other result. Rust priorities use
                // a smaller range than upstream's, and zoxide's output order
                // is preserved when multiple scores cap at the same value.
                priority: Some(if immediate_child {
                    100
                } else {
                    score.clamp(0.0, 98.0) as u8
                }),
                ..Default::default()
            });
        }
    }
    if legacy {
        out.extend(legacy_z_history(&terms));
    }
    for entry in list_paths(cwd, "", true, false) {
        if !entry.name.starts_with('.')
            && (!legacy || terms.iter().all(|term| entry.name.contains(term)))
        {
            out.push(Suggestion {
                name: entry.name,
                description: Some("Score: 0".into()),
                icon: Some("📁".into()),
                priority: Some(if legacy { 50 } else { 99 }),
                ..Default::default()
            });
        }
    }
    out
}

fn legacy_z_history(terms: &[&str]) -> Vec<Suggestion> {
    let path = ["ZSHZ_DATA", "_Z_DATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| crate::paths::home().map(|home| home.join(".z")));
    let Some(data) = path.and_then(|path| read_bounded_file(&path, MAX_GENERATOR_OUTPUT as u64))
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&data)
        .lines()
        .filter_map(|line| {
            let mut fields = line.rsplitn(3, '|');
            let _time = fields.next()?.parse::<u64>().ok()?;
            let weight = fields.next()?.parse::<f64>().ok()?;
            let path = fields.next()?;
            let name = Path::new(path).file_name()?.to_str()?;
            if !weight.is_finite()
                || !Path::new(path).is_absolute()
                || terms.contains(&name)
                || !terms.iter().all(|term| path.contains(term))
            {
                return None;
            }
            Some(Suggestion {
                name: name.into(),
                description: Some(path.into()),
                icon: Some("📁".into()),
                priority: Some((75.0 + weight.clamp(0.0, 9000.0) * 25.0 / 9000.0) as u8),
                ..Default::default()
            })
        })
        .collect()
}

fn rust_toolchains(cwd: &str, prefix: &str) -> Vec<Suggestion> {
    if !prefix.is_empty() && !prefix.starts_with('+') {
        return Vec::new();
    }
    let command = Invocation::Argv(["rustup", "toolchain", "list"].map(String::from).to_vec());
    let cache = CacheSpec {
        by_dir: false,
        ..Default::default()
    };
    run_shell_line(&command, "\n", 2_000, Some(&cache), cwd)
        .into_iter()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let toolchain = words.next()?;
            // Rustup prints a sentence, not a toolchain, when its home is empty.
            if words.next().is_some_and(|word| !word.starts_with('(')) {
                return None;
            }
            let name = [
                "-x86_64-",
                "-aarch64-",
                "-i686-",
                "-arm-",
                "-armv7-",
                "-riscv64gc-",
                "-powerpc64le-",
                "-s390x-",
                "-loongarch64-",
            ]
            .into_iter()
            .find_map(|target| toolchain.split_once(target).map(|(name, _)| name))
            .unwrap_or(toolchain);
            Some(Suggestion {
                name: format!("+{name}"),
                description: Some(line),
                icon: Some("🧰".into()),
                priority: Some(45),
                ..Default::default()
            })
        })
        .collect()
}

fn docker_images(cwd: &str) -> Vec<Suggestion> {
    let argv = [
        "docker",
        "images",
        "--format",
        "{{.Repository}} {{.Size}} {{.Tag}} {{.ID}}",
    ];
    let command = Invocation::Argv(argv.map(String::from).to_vec());
    run_shell_line(&command, "\n", 5_000, None, cwd)
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let repository = fields.next()?;
            let size = fields.next()?;
            let tag = fields.next()?;
            let id = fields.next()?;
            if repository == "<none>" || fields.next().is_some() {
                return None;
            }
            Some(Suggestion {
                name: repository.into(),
                description: Some(format!("{id}@{tag} - {size}")),
                icon: Some("🐳".into()),
                priority: Some(60),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(unix)]
fn local_login_names() -> Vec<Suggestion> {
    let Some(bytes) = read_bounded_file(Path::new("/etc/passwd"), MAX_MANIFEST_BYTES) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (name, _) = line.split_once(':')?;
            if name.is_empty() || name.starts_with(['#', '+', '-']) {
                return None;
            }
            Some(Suggestion {
                name: name.into(),
                icon: Some("👤".into()),
                priority: Some(60),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(not(unix))]
fn local_login_names() -> Vec<Suggestion> {
    Vec::new()
}

fn npm_workspaces(cwd: &str) -> Vec<Suggestion> {
    for dir in Path::new(if cwd.is_empty() { "." } else { cwd }).ancestors() {
        let Some(text) = read_project_file(&dir.join("package.json"), MAX_MANIFEST_BYTES) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(workspaces) = manifest["workspaces"]
            .as_array()
            .or_else(|| manifest["workspaces"]["packages"].as_array())
        else {
            continue;
        };
        return workspaces
            .iter()
            .filter_map(|workspace| {
                Some(Suggestion {
                    name: workspace.as_str()?.into(),
                    description: Some("Workspaces".into()),
                    ..Default::default()
                })
            })
            .collect();
    }
    Vec::new()
}

fn encode_url_part(input: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte.into());
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn npm_registry(cwd: &str, prefix: &str) -> Vec<Suggestion> {
    if prefix.is_empty()
        || prefix.starts_with('-')
        || prefix.len() > 256
        || prefix.chars().any(char::is_whitespace)
    {
        return Vec::new();
    }
    let version = prefix
        .strip_prefix('@')
        .unwrap_or(prefix)
        .split_once('@')
        .map(|(name, _)| {
            if prefix.starts_with('@') {
                format!("@{name}")
            } else {
                name.to_owned()
            }
        });
    let (url, accept) = match &version {
        Some(pkg) => (
            format!("https://registry.npmjs.org/{}", encode_url_part(pkg)),
            "Accept: application/vnd.npm.install-v1+json",
        ),
        None => (
            format!(
                "https://api.npms.io/v2/search/suggestions?size=20&q={}",
                encode_url_part(prefix)
            ),
            "Accept: application/json",
        ),
    };
    let argv = [
        "curl",
        "--disable",
        "--fail",
        "--silent",
        "--max-time",
        "4",
        "--proto",
        "=https",
        "-H",
        accept,
        &url,
    ];
    let cache = CacheSpec {
        ttl_secs: if version.is_some() { 172_800 } else { 60 },
        by_dir: false,
        ..Default::default()
    };
    // Parse the whole document since curl may return formatted JSON.
    let raw = run_shell_line(
        &Invocation::Argv(argv.map(String::from).to_vec()),
        "\0",
        5_000,
        Some(&cache),
        cwd,
    );
    let Ok(value) =
        serde_json::from_str::<serde_json::Value>(raw.first().map_or("", String::as_str))
    else {
        return Vec::new();
    };
    if let Some(pkg) = version {
        let mut out = Vec::new();
        if let Some(tags) = value["dist-tags"].as_object() {
            out.extend(tags.iter().map(|(tag, version)| Suggestion {
                name: tag.clone(),
                insert_value: Some(format!("{pkg}@{tag}")),
                match_name: Some(format!("{pkg}@{tag}")),
                description: version.as_str().map(str::to_owned),
                priority: Some(60),
                ..Default::default()
            }));
        }
        if let Some(versions) = value["versions"].as_object() {
            let mut versions: Vec<_> = versions.keys().map(String::as_str).collect();
            versions.sort_by_cached_key(|version| {
                let (base, pre) = version.split_once('-').unwrap_or((version, ""));
                let mut parts = base.split('.').map(|n| n.parse::<u64>().unwrap_or(0));
                std::cmp::Reverse((
                    parts.next(),
                    parts.next(),
                    parts.next(),
                    pre.is_empty(),
                    pre.to_owned(),
                ))
            });
            out.extend(versions.into_iter().map(|version| Suggestion {
                name: version.into(),
                insert_value: Some(format!("{pkg}@{version}")),
                match_name: Some(format!("{pkg}@{version}")),
                priority: Some(60),
                ..Default::default()
            }));
        }
        return out;
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|result| {
            let package = &result["package"];
            Some(Suggestion {
                name: package["name"].as_str()?.into(),
                description: package["description"].as_str().map(str::to_owned),
                priority: Some(60),
                ..Default::default()
            })
        })
        .collect()
}

fn file_exists_then(
    rel_path: &str,
    content_contains: Option<&str>,
    sub: &Subcommand,
    cwd: &str,
) -> Vec<Suggestion> {
    let cwd_path = if cwd.is_empty() { "." } else { cwd };
    let target = std::path::Path::new(cwd_path).join(rel_path);
    if !target.exists() {
        return Vec::new();
    }
    if let Some(needle) = content_contains {
        let Some(contents) = read_project_file(&target, MAX_PROBE_FILE_BYTES) else {
            return Vec::new();
        };
        if !contents.contains(needle) {
            return Vec::new();
        }
    }
    sub.names
        .iter()
        .map(|n| Suggestion {
            name: n.clone(),
            description: sub.description.clone(),
            suggestion_type: SuggestionType::Subcommand,
            priority: Some(60),
            ..Default::default()
        })
        .collect()
}

/// Hardcoded set of node CLIs that are interesting as `pnpm/yarn/bun`
/// subcommands. Filtered against package.json deps or node_modules/.bin
/// entries to surface useful loadable specs.
const NODE_CLIS: &[&str] = &[
    "vite",
    "vitest",
    "jest",
    "mocha",
    "ava",
    "tap",
    "eslint",
    "prettier",
    "tsc",
    "tslint",
    "webpack",
    "rollup",
    "parcel",
    "esbuild",
    "swc",
    "next",
    "nuxt",
    "remix",
    "astro",
    "gatsby",
    "vue-cli-service",
    "nx",
    "playwright",
    "cypress",
    "storybook",
    "babel",
    "babel-node",
    "ts-node",
    "tsx",
    "tap-spec",
    "nyc",
    "lerna",
    "rush",
    "pnpm",
    "yarn",
    "bun",
    "node",
    "nodemon",
    "concurrently",
    "husky",
    "lint-staged",
    "rimraf",
    "cross-env",
    "del-cli",
    "serve",
    "http-server",
    "browser-sync",
    "stylelint",
    "postcss",
    "sass",
    "less",
    "tailwindcss",
    "fastify",
    "nest",
    "hardhat",
    "truffle",
    "ganache",
    "fauna-shell",
    "wrangler",
    "vercel",
    "netlify",
    "supabase",
    "amplify",
    "firebase",
    "convex",
    "drizzle-kit",
    "prisma",
    "knex",
    "sequelize",
    "mongoose",
];

fn is_known_node_cli(name: &str) -> bool {
    NODE_CLIS.contains(&name)
}

/// Largest manifest (`package.json`, `Cargo.toml`) read for suggestions. Real
/// ones are a few KiB; a monorepo root stays well under this.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Largest file `FileExistsThen` scans for its marker (`manage.py`).
const MAX_PROBE_FILE_BYTES: u64 = 256 * 1024;

/// Read a file from the working directory for completion, or `None`.
///
/// These files belong to whoever authored the directory, and are read on
/// every keystroke. `read_to_string` after `exists()` followed a symlink to
/// `/dev/zero` and grew its buffer until the allocation aborted the wrapper
/// (taking the user's shell session with it), and blocked forever opening a
/// FIFO. Only regular files up to `limit` bytes are read. On Unix the file is
/// opened non-blocking so a FIFO cannot stall the open, and the type and size
/// checks use the opened handle, so swapping the path afterwards changes
/// nothing.
fn read_project_file(path: &Path, limit: u64) -> Option<String> {
    let bytes = read_bounded_file(path, limit)?;
    String::from_utf8(bytes).ok()
}

/// Bounded, regular-file-only read shared by user-controlled paths
/// (spec dirs, history files — #103, extending the #87 fix). Opening a
/// FIFO blocks forever and an unbounded read of `/dev/zero` grows until
/// OOM, so: open non-blocking on Unix, require a regular file via the
/// opened handle, and cap bytes read at `limit`.
pub(crate) fn read_bounded_file(path: &Path, limit: u64) -> Option<Vec<u8>> {
    read_bounded_file_result(path, limit).ok()
}

/// Preserve I/O errors for configuration diagnostics and profile editors:
/// only NotFound may mean an absent file, never a rejected existing input.
pub(crate) fn read_bounded_file_result(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind, Read};
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    let too_large = || {
        Error::new(
            ErrorKind::InvalidData,
            format!("file exceeds {limit} byte read limit"),
        )
    };
    if meta.len() > limit {
        return Err(too_large());
    }
    // The size can still grow between the check and the read.
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

fn project_file_suggestions(reader: ProjectFileReader, cwd: &str) -> Vec<Suggestion> {
    match reader {
        ProjectFileReader::PackageJsonScripts => package_json_scripts(cwd),
        ProjectFileReader::PackageJsonNodeClis => package_json_node_clis(cwd),
        ProjectFileReader::NodeModulesBinaries => node_modules_binaries(cwd, true),
        ProjectFileReader::CargoWorkspaceMembers => cargo_workspace_members(cwd),
    }
}

fn package_json_scripts(cwd: &str) -> Vec<Suggestion> {
    let pkg_path =
        std::path::Path::new(if cwd.is_empty() { "." } else { cwd }).join("package.json");
    let Some(text) = read_project_file(&pkg_path, MAX_MANIFEST_BYTES) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(scripts) = value.get("scripts").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    scripts
        .iter()
        .map(|(name, body)| Suggestion {
            name: name.clone(),
            description: body.as_str().map(|s| s.to_string()),
            suggestion_type: SuggestionType::Arg,
            priority: Some(70),
            ..Default::default()
        })
        .collect()
}

fn package_json_node_clis(cwd: &str) -> Vec<Suggestion> {
    let pkg_path =
        std::path::Path::new(if cwd.is_empty() { "." } else { cwd }).join("package.json");
    let Some(text) = read_project_file(&pkg_path, MAX_MANIFEST_BYTES) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(deps) = value.get(key).and_then(|v| v.as_object()) {
            for dep_name in deps.keys() {
                if is_known_node_cli(dep_name) {
                    names.insert(dep_name.clone());
                }
            }
        }
    }
    names
        .into_iter()
        .map(|name| Suggestion {
            name: name.clone(),
            description: Some(format!("Run {} via the package manager", name)),
            suggestion_type: SuggestionType::Subcommand,
            priority: Some(65),
            ..Default::default()
        })
        .collect()
}

fn node_modules_binaries(cwd: &str, known_only: bool) -> Vec<Suggestion> {
    let mut dir = std::path::PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    if let Ok(canon) = dir.canonicalize() {
        dir = canon;
    }
    loop {
        let candidate = dir.join("node_modules").join(".bin");
        if candidate.is_dir() {
            let Ok(entries) = std::fs::read_dir(&candidate) else {
                return Vec::new();
            };
            return entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !known_only || is_known_node_cli(n))
                .map(|name| Suggestion {
                    name: name.clone(),
                    description: Some(format!("Run {} from node_modules", name)),
                    suggestion_type: SuggestionType::Subcommand,
                    priority: Some(65),
                    ..Default::default()
                })
                .collect();
        }
        if !dir.pop() {
            return Vec::new();
        }
    }
}

fn cargo_workspace_members(cwd: &str) -> Vec<Suggestion> {
    let path = std::path::Path::new(if cwd.is_empty() { "." } else { cwd }).join("Cargo.toml");
    let Some(text) = read_project_file(&path, MAX_MANIFEST_BYTES) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return Vec::new();
    };
    let Some(members) = value
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    else {
        return Vec::new();
    };
    members
        .iter()
        .filter_map(|m| m.as_str().map(String::from))
        .map(|name| Suggestion {
            name: name.clone(),
            suggestion_type: SuggestionType::Arg,
            priority: Some(60),
            ..Default::default()
        })
        .collect()
}

fn template_suggestions(
    tpl: Template,
    cwd: &str,
    prefix: &str,
    include_history: bool,
    from_generator: bool,
    help_subcommands: &[Subcommand],
    shell: Option<Shell>,
) -> Vec<Suggestion> {
    match tpl {
        // Only the FOLDERS generator appends `..` (matches upstream `cd `);
        // filepaths generators (e.g. `git checkout <file>`) do not, so
        // filepaths never sets add_parent.
        Template::Filepaths => list_paths(cwd, prefix, false, false),
        Template::Folders => list_paths(cwd, prefix, true, from_generator),
        // Offline `complete` has no live session history (upstream returns
        // none here); only surface history in the interactive engine.
        Template::History if !include_history => Vec::new(),
        // Read the WRAPPED shell's history. The no-arg loader resolves the
        // shell of the *parent* process, so `is start --shell fish` launched
        // from bash served ssh/scp/rsync completions out of ~/.bash_history
        // and never saw anything typed in the wrapped session.
        Template::History => crate::history::load_for(shell.unwrap_or_else(crate::shell::detect))
            .into_iter()
            .map(|h| Suggestion {
                name: h,
                suggestion_type: SuggestionType::Arg,
                priority: Some(55),
                ..Default::default()
            })
            .collect(),
        // Fig's `help` template completes the parent command's subcommands.
        // Returning nothing meant `<cmd> help <TAB>` offered nothing at all.
        Template::Help => help_subcommands
            .iter()
            .filter(|s| !s.hidden)
            .filter_map(|s| {
                let name = s.names.first()?;
                Some(Suggestion {
                    name: name.clone(),
                    all_names: s.names.clone(),
                    description: s.description.clone(),
                    suggestion_type: SuggestionType::Subcommand,
                    priority: Some(s.priority.unwrap_or(50)),
                    icon: s.icon.clone(),
                    deprecated: s.deprecated,
                    ..Default::default()
                })
            })
            .collect(),
    }
}

/// Split a path prefix into its directory part and the basename being typed.
/// Windows accepts `\\` as a separator, so `src\\ma` must split at the
/// backslash rather than being treated as one long filename.
pub(crate) fn split_path_prefix(prefix: &str) -> (&str, &str) {
    let sep = if cfg!(windows) {
        prefix.rfind(['/', '\\'])
    } else {
        prefix.rfind('/')
    };
    match sep {
        // Keep the separator so `/foo` retains its filesystem root and
        // `C:\foo` retains the drive root on Windows (#123).
        Some(idx) => (&prefix[..=idx], &prefix[idx + 1..]),
        None => ("", prefix),
    }
}

/// Expand a leading `~` (or `~/...`) to the user's home directory. Without
/// this, `ls ~/Doc` searched for a literal directory named `~`.
fn expand_tilde(dir: &str) -> Option<std::path::PathBuf> {
    let rest = dir.strip_prefix('~')?;
    if !(rest.is_empty() || rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\'))) {
        // `~user` — we do not resolve other users' homes.
        return None;
    }
    let home = crate::paths::home()?;
    let rest = rest.trim_start_matches(['/', '\\']);
    Some(if rest.is_empty() {
        home
    } else {
        home.join(rest)
    })
}

fn list_paths(cwd: &str, prefix: &str, dirs_only: bool, add_parent: bool) -> Vec<Suggestion> {
    let base = Path::new(cwd);
    let (dir, file_prefix) = split_path_prefix(prefix);
    let target = if let Some(expanded) = expand_tilde(dir) {
        expanded
    } else if dir.is_empty() {
        base.to_path_buf()
    } else {
        base.join(dir)
    };
    let Ok(entries) = std::fs::read_dir(&target) else {
        return Vec::new();
    };
    // Upstream matches path basenames by case-insensitive SUBSTRING, not
    // prefix — `ls fil` surfaces `afile.txt`, `ls xt` surfaces `*.txt`
    // (verified against the installed binary).
    let needle = file_prefix.to_lowercase();
    let mut pairs: Vec<(String, bool)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_lowercase().contains(&needle) {
            continue;
        }
        // Directory entries normally carry their type already. Only symlinks
        // need a metadata lookup to follow their target (otherwise `cd`
        // drops directory links). Avoid a stat for every ordinary entry.
        let is_dir = match entry.file_type() {
            Ok(kind) if !kind.is_symlink() => kind.is_dir(),
            _ => std::fs::metadata(entry.path())
                .map(|m| m.is_dir())
                .unwrap_or(false),
        };
        if dirs_only && !is_dir {
            continue;
        }
        pairs.push((name, is_dir));
    }
    if dirs_only {
        // Folders template (`cd `): upstream lists non-hidden dirs first,
        // then hidden, each alphabetical. (Safe for `ls`, which lists via the
        // filepaths template first; the folders-template dirs dedup away.)
        pairs.sort_by(|a, b| {
            a.0.starts_with('.')
                .cmp(&b.0.starts_with('.'))
                .then_with(|| a.0.cmp(&b.0))
        });
    } else {
        // Filepaths template: plain alphabetical (hidden dotfiles come first
        // since `.` < any letter in ASCII). Files and directories interleave —
        // upstream does NOT group dirs first, and does NOT add `..` here (that
        // is `cd`-spec-specific).
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let mut out = Vec::with_capacity(pairs.len());
    for (name, is_dir) in pairs {
        let joined = if dir.is_empty() {
            name
        } else {
            format!("{dir}{name}")
        };
        // Upstream emits directory names WITHOUT a trailing slash (`adir`,
        // not `adir/`); the Folder type still suppresses the post-accept
        // space in the popup, so deeper paths work the same.
        let (final_name, stype) = if is_dir {
            (joined, SuggestionType::Folder)
        } else {
            (joined, SuggestionType::File)
        };
        out.push(Suggestion {
            name: final_name,
            suggestion_type: stype,
            priority: Some(55),
            ..Default::default()
        });
    }
    // The generator form of filepaths/folders appends the parent dir `..`
    // (priority 50, so it sorts after the listed entries — matching upstream
    // `python `/`node `/`cd `). Honor the same substring filter, root only.
    if add_parent && dir.is_empty() && "..".contains(&needle) {
        out.push(Suggestion {
            name: "..".to_string(),
            suggestion_type: SuggestionType::Folder,
            priority: Some(50),
            ..Default::default()
        });
    }
    out
}

fn glob_paths(pattern: &str, cwd: &str) -> Vec<Suggestion> {
    let script_str = format!("ls -d {} 2>/dev/null", pattern);
    run_shell_line(&Invocation::Shell(script_str), "\n", 5000, None, cwd)
        .into_iter()
        .map(|name| Suggestion {
            name,
            suggestion_type: SuggestionType::File,
            priority: Some(60),
            ..Default::default()
        })
        .collect()
}

fn run_script(
    input: &ScriptInput,
    split_on: Option<&str>,
    post: &PostProcess,
    timeout_ms: u32,
    cache: Option<&CacheSpec>,
    cwd: &str,
    _prefix: &str,
) -> Vec<Suggestion> {
    // An argv generator names a program and its arguments. Re-serializing it
    // into a shell string changed its semantics (a literal `*` would glob, a
    // literal `$X` would expand) and could not run at all on native Windows,
    // where there is no `sh`.
    let command = match input {
        ScriptInput::Shell { script } => Invocation::Shell(script.clone()),
        ScriptInput::Argv { argv } => Invocation::Argv(argv.clone()),
        ScriptInput::FnTemplate { template } => {
            // Phase 4 will expand {tokens[N]} placeholders. For now treat as
            // a literal shell command.
            Invocation::Shell(template.clone())
        }
    };
    // `PostProcess::Split { sep }` documents that it *overrides* the
    // generator's `split_on`. It was silently ignored, and its `sep` never read.
    let sep = match post {
        PostProcess::Split { sep } => sep.as_str(),
        _ => split_on.unwrap_or("\n"),
    };
    let raw = run_shell_line(&command, sep, timeout_ms, cache, cwd);
    apply_post_process(raw, post)
}

/// How a generator's script is executed.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum Invocation {
    /// A shell fragment; needs a shell to interpret it.
    Shell(String),
    /// A program and its arguments; spawned directly.
    Argv(Vec<String>),
}

fn run_shell_line(
    command: &Invocation,
    sep: &str,
    timeout_ms: u32,
    cache: Option<&CacheSpec>,
    cwd: &str,
) -> Vec<String> {
    let request = current_request();
    if request.as_ref().is_some_and(Cancellation::is_cancelled) {
        return Vec::new();
    }
    let timeout_ms = timeout_ms.max(1);
    // No policy, or a zero TTL, bypasses both cache reads and cache writes.
    // In particular another generator's longer TTL cannot override this call.
    let Some(spec) = cache.filter(|spec| !spec.ttl().is_zero()) else {
        return run_and_split(command, sep, timeout_ms, cwd).unwrap_or_default();
    };

    // Cache values depend on invocation kind, argv boundaries, separator and
    // (when requested) cwd (#113/#117). Freshness belongs to the caller's
    // policy, rather than whichever generator populated the shared snapshot.
    let key = CacheKey {
        command: command.clone(),
        cwd: spec.by_dir.then(|| cwd.to_string()),
        separator: sep.to_string(),
    };
    let entry = {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&key) {
            entry.clone()
        } else if cache.len() < MAX_CACHE_ENTRIES {
            cache.entry(key).or_default().clone()
        } else if let Some(idle) = cache
            .iter()
            .find(|(_, entry)| Arc::strong_count(entry) == 1)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&idle);
            cache.entry(key).or_default().clone()
        } else {
            // Never evict active flights. Run this request without a cache
            // entry if all slots are busy.
            drop(cache);
            return run_and_split(command, sep, timeout_ms, cwd).unwrap_or_default();
        }
    };
    let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = &state.cached
        && cached.at.elapsed() < spec.ttl()
    {
        return (*cached.values).clone();
    }

    let stale = match spec.strategy {
        CacheStrategy::StaleWhileRevalidate => {
            state.cached.as_ref().map(|cached| cached.values.clone())
        }
        CacheStrategy::MaxAge => None,
    };
    if let Some(stale) = &stale
        && !state.in_flight.is_empty()
    {
        // One ongoing execution is enough for stale callers, regardless of
        // its timeout. Their next request can retry if that refresh fails.
        return (**stale).clone();
    }
    let (flight, launch) = match state.in_flight.get(&timeout_ms) {
        Some(flight) => (flight.clone(), false),
        None => {
            let flight = Arc::new(CacheFlight {
                refresh: stale.is_some(),
                ..Default::default()
            });
            state.in_flight.insert(timeout_ms, flight.clone());
            (flight, true)
        }
    };
    if stale.is_none() {
        flight.waiters.fetch_add(1, Ordering::Relaxed);
    }
    drop(state);

    if launch {
        let entry_for_thread = entry.clone();
        let flight_for_thread = flight.clone();
        let command = command.clone();
        let sep = sep.to_string();
        let cwd = cwd.to_string();
        if std::thread::Builder::new()
            .name("is-generator-cache".into())
            .spawn(move || {
                let output = with_request(Some(flight_for_thread.cancel.clone()), || {
                    run_and_split(&command, &sep, timeout_ms, &cwd).map(Arc::new)
                });
                finish_cache_run(&entry_for_thread, &flight_for_thread, timeout_ms, output);
            })
            .is_err()
        {
            finish_cache_run(&entry, &flight, timeout_ms, None);
        }
    }
    if let Some(stale) = stale {
        return (*stale).clone();
    }

    let output = flight.wait(timeout_ms, request.as_ref());
    // Serialize the last departure with registration of another waiter.
    let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
    if flight.waiters.fetch_sub(1, Ordering::Relaxed) == 1
        && !flight.refresh
        && state
            .in_flight
            .get(&timeout_ms)
            .is_some_and(|current| Arc::ptr_eq(current, &flight))
    {
        flight.cancel.cancel();
        state.in_flight.remove(&timeout_ms);
    }
    output.map(|v| (*v).clone()).unwrap_or_default()
}

fn finish_cache_run(
    entry: &CacheEntry,
    flight: &CacheFlight,
    timeout_ms: u32,
    output: ScriptOutput,
) {
    let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
    if state
        .in_flight
        .get(&timeout_ms)
        .is_some_and(|current| std::ptr::eq(current.as_ref(), flight))
    {
        if let Some(values) = &output {
            state.cached = Some(CachedOutput {
                at: Instant::now(),
                values: values.clone(),
            });
        }
        state.in_flight.remove(&timeout_ms);
    }
    drop(state);
    *flight.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(output);
    flight.done.notify_all();
}

fn run_and_split(
    command: &Invocation,
    sep: &str,
    timeout_ms: u32,
    cwd: &str,
) -> Option<Vec<String>> {
    run_command_with_timeout(command, cwd, timeout_ms).map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .split(sep)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Build the `Command` for an invocation. Argv generators are spawned
/// directly — never re-serialized through a shell.
fn build_command(invocation: &Invocation) -> Option<Command> {
    match invocation {
        Invocation::Argv(argv) => {
            let (program, args) = argv.split_first()?;
            let mut command = Command::new(program);
            command.args(args);
            Some(command)
        }
        Invocation::Shell(script) => {
            // Native Windows has no `sh`; a shell generator there must run
            // under `cmd`, otherwise the spawn fails and the generator
            // silently produces no completions.
            #[cfg(windows)]
            {
                let mut command = Command::new("cmd");
                command.arg("/C").arg(script);
                Some(command)
            }
            #[cfg(not(windows))]
            {
                let mut command = Command::new("sh");
                command.arg("-c").arg(script);
                Some(command)
            }
        }
    }
}

fn run_command_with_timeout(
    invocation: &Invocation,
    cwd: &str,
    timeout_ms: u32,
) -> Option<Vec<u8>> {
    try_run_command(invocation, cwd, timeout_ms).ok().flatten()
}

fn try_run_command(
    invocation: &Invocation,
    cwd: &str,
    timeout_ms: u32,
) -> std::io::Result<Option<Vec<u8>>> {
    let request = current_request();
    if request.as_ref().is_some_and(Cancellation::is_cancelled) {
        return Ok(None);
    }
    let Some(mut command) = build_command(invocation) else {
        return Ok(None);
    };
    command
        .current_dir(if cwd.is_empty() { "." } else { cwd })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let child = command.spawn()?;
    let timeout = Duration::from_millis(timeout_ms.max(1) as u64);
    Ok(capture_generator(child, timeout, request.as_ref()))
}

#[cfg(not(windows))]
fn capture_generator(
    mut child: std::process::Child,
    timeout: Duration,
    request: Option<&Cancellation>,
) -> Option<Vec<u8>> {
    let mut stdout = child.stdout.take()?;
    // The reader runs on its own thread and reports through a channel, so the
    // wait for it can be bounded. `join()` could block forever: a backgrounded
    // grandchild inherits the stdout pipe and holds its write end open long
    // after the direct child has exited, so `read_to_end` never returns and
    // the generator outlived its timeout.
    //
    // The read is capped too. The timeout bounds how long a generator runs,
    // not how much it prints in that time, and an abandoned reader on an
    // escaped descendant kept accumulating after the timeout fired.
    let (tx, rx) = std::sync::mpsc::channel();
    let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let overflowed = overflowed.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let limit = MAX_GENERATOR_OUTPUT as u64 + 1;
            let _ =
                std::io::Read::read_to_end(&mut std::io::Read::take(&mut stdout, limit), &mut buf);
            if buf.len() > MAX_GENERATOR_OUTPUT {
                buf = Vec::new();
                overflowed.store(true, std::sync::atomic::Ordering::Release);
            }
            let _ = tx.send(buf);
        });
    }
    let overflowed = || overflowed.load(std::sync::atomic::Ordering::Acquire);

    let start = Instant::now();
    loop {
        // Over the cap: the output is unusable, so stop the generator (and
        // its process group) now instead of letting it block on a full pipe
        // until the timeout.
        if overflowed() || request.is_some_and(Cancellation::is_cancelled) {
            kill_generator_child(&mut child);
            let _ = child.wait();
            return None;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // The shell may have exited while a background descendant
                // still owns stdout. End that process group before waiting
                // for EOF, preserving the output already written (#118).
                kill_generator_child(&mut child);
                // Killing the process group closes any inherited write end, so
                // the reader unblocks; the deadline covers the case where a
                // descendant escaped the group.
                let grace = timeout
                    .saturating_sub(start.elapsed())
                    .max(Duration::from_millis(50));
                if !status.success() {
                    return None;
                }
                let deadline = Instant::now() + grace;
                while !request.is_some_and(Cancellation::is_cancelled) {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let wait = if request.is_some() {
                        remaining.min(POLL_INTERVAL)
                    } else {
                        remaining
                    };
                    match rx.recv_timeout(wait) {
                        Ok(bytes) => return (!overflowed()).then_some(bytes),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                return None;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_generator_child(&mut child);
                    let _ = child.wait();
                    // Reader may still be blocked on an escaped descendant;
                    // abandon it rather than hang the completion.
                    let _ = rx.recv_timeout(Duration::from_millis(50));
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                kill_generator_child(&mut child);
                let _ = child.wait();
                let _ = rx.recv_timeout(Duration::from_millis(50));
                return None;
            }
        }
    }
}

#[cfg(windows)]
fn capture_generator(
    mut child: std::process::Child,
    timeout: Duration,
    request: Option<&Cancellation>,
) -> Option<Vec<u8>> {
    use std::io::{ErrorKind, Read};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut stdout = child.stdout.take()?;
    let start = Instant::now();
    let mut output = Vec::new();
    let mut buf = [0; 8192];
    let result = loop {
        if request.is_some_and(Cancellation::is_cancelled) {
            break None;
        }
        let exited = match child.try_wait() {
            Ok(Some(status)) if status.success() => true,
            Ok(Some(_)) | Err(_) => break None,
            Ok(None) => false,
        };
        let mut available = 0;
        // This handle has one reader. Read only bytes reported available so
        // a descendant retaining its write handle cannot strand a reader.
        if unsafe {
            PeekNamedPipe(
                stdout.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        } == 0
            && std::io::Error::last_os_error().kind() != ErrorKind::BrokenPipe
        {
            break None;
        }
        if available == 0 && exited {
            break Some(output);
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break None;
        }
        if available == 0 {
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
            continue;
        }
        let n = (available as usize)
            .min(buf.len())
            .min(MAX_GENERATOR_OUTPUT + 1 - output.len());
        match stdout.read(&mut buf[..n]) {
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
                if output.len() > MAX_GENERATOR_OUTPUT {
                    break None;
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => break None,
        }
    };
    drop(stdout);
    if result.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

/// Most bytes of generator stdout kept. Completion lists are a few KiB; even
/// every installable package on a distribution (`apt-cache pkgnames`) is
/// around 1 MiB. Output over the cap is discarded as a failed generator.
const MAX_GENERATOR_OUTPUT: usize = 4 * 1024 * 1024;

#[cfg(not(windows))]
fn kill_generator_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn apply_post_process(raw: Vec<String>, post: &PostProcess) -> Vec<Suggestion> {
    match post {
        PostProcess::None {} | PostProcess::Split { .. } => raw
            .into_iter()
            .map(clean_raw_line)
            .filter(|n| !n.is_empty())
            .map(|name| Suggestion {
                name,
                suggestion_type: SuggestionType::Arg,
                priority: Some(60),
                ..Default::default()
            })
            .collect(),
        PostProcess::Pattern { inner } => apply_pattern(raw, inner),
        PostProcess::Fn { .. } => Vec::new(), // phase 6
    }
}

/// Trim leading whitespace and common decoration prefixes from raw
/// script output. Upstream's spec files often set `postProcess` to a
/// JS callback that does this cleanup; our extractor can't evaluate
/// those callbacks, so we compensate with a safe default here: strip
/// leading whitespace, leading `* ` (git branch current marker), and
/// trailing whitespace. This makes `git checkout <TAB>` return
/// `main` instead of `* main`, matching upstream's behavior.
fn clean_raw_line(line: String) -> String {
    let trimmed = line.trim();
    let stripped = trimmed.strip_prefix("* ").unwrap_or(trimmed);
    stripped.trim().to_string()
}

fn apply_pattern(raw: Vec<String>, kind: &PostProcessKind) -> Vec<Suggestion> {
    match kind {
        PostProcessKind::SplitLines {} => raw_to_suggestions(raw),
        PostProcessKind::SplitLinesFiltered { skip_prefixes } => {
            let filtered: Vec<String> = raw
                .into_iter()
                .filter(|line| !skip_prefixes.iter().any(|p| line.starts_with(p)))
                .collect();
            raw_to_suggestions(filtered)
        }
        PostProcessKind::JsonParse {} => {
            let blob = raw.join("\n");
            json_to_suggestions(&blob, None)
        }
        PostProcessKind::JsonPath { path } => {
            let blob = raw.join("\n");
            json_to_suggestions(&blob, Some(path))
        }
        PostProcessKind::GitBranches {} => raw
            .into_iter()
            .map(|line| line.trim_start_matches('*').trim().to_string())
            .filter(|line| !line.is_empty() && line != "HEAD")
            .map(|line| {
                let stripped = line
                    .strip_prefix("remotes/")
                    .map(|s| s.to_string())
                    .unwrap_or(line);
                Suggestion {
                    name: stripped,
                    suggestion_type: SuggestionType::Arg,
                    priority: Some(60),
                    ..Default::default()
                }
            })
            .collect(),
        PostProcessKind::KeyValueColon {} => raw
            .into_iter()
            .filter_map(|line| {
                let mut parts = line.splitn(2, ':');
                let key = parts.next()?.trim().to_string();
                let val = parts.next().map(|s| s.trim().to_string());
                if key.is_empty() {
                    return None;
                }
                Some(Suggestion {
                    name: key,
                    description: val,
                    suggestion_type: SuggestionType::Arg,
                    priority: Some(60),
                    ..Default::default()
                })
            })
            .collect(),
        PostProcessKind::GitBranchList {} => raw
            .into_iter()
            .map(|line| {
                let is_current = line.trim_start().starts_with('*');
                (is_current, clean_raw_line(line))
            })
            .filter(|(_, name)| {
                !name.is_empty() && !name.contains(" -> ") && !name.starts_with("(HEAD ")
            })
            .map(|(is_current, name)| Suggestion {
                name,
                suggestion_type: SuggestionType::Arg,
                priority: Some(if is_current { 100 } else { 75 }),
                ..Default::default()
            })
            .collect(),
        PostProcessKind::FirstTokenRest {} => raw
            .into_iter()
            .filter_map(|line| {
                let line = line.trim();
                let mut it = line.splitn(2, char::is_whitespace);
                let name = it.next()?.trim().to_string();
                if name.is_empty() {
                    return None;
                }
                let desc = it
                    .next()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                Some(Suggestion {
                    name,
                    description: desc,
                    suggestion_type: SuggestionType::Arg,
                    priority: Some(60),
                    ..Default::default()
                })
            })
            .collect(),
        PostProcessKind::TableColumn { index, sep } => raw
            .into_iter()
            .filter_map(|line| {
                let cols: Vec<&str> = if sep.is_empty() {
                    line.split_whitespace().collect()
                } else {
                    line.split(sep.as_str()).collect()
                };
                let val = cols.get(*index as usize)?.trim().to_string();
                if val.is_empty() {
                    return None;
                }
                Some(Suggestion {
                    name: val,
                    suggestion_type: SuggestionType::Arg,
                    priority: Some(60),
                    ..Default::default()
                })
            })
            .collect(),
    }
}

fn raw_to_suggestions(raw: Vec<String>) -> Vec<Suggestion> {
    raw.into_iter()
        .filter(|s| !s.is_empty())
        .map(|name| Suggestion {
            name,
            suggestion_type: SuggestionType::Arg,
            priority: Some(60),
            ..Default::default()
        })
        .collect()
}

fn json_to_suggestions(blob: &str, path: Option<&str>) -> Vec<Suggestion> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(blob) else {
        return Vec::new();
    };
    let target = match path {
        Some(p) => {
            let ptr = if p.starts_with('/') {
                p.to_string()
            } else {
                format!("/{}", p.replace('.', "/"))
            };
            match value.pointer(&ptr) {
                Some(v) => v,
                None => return Vec::new(),
            }
        }
        None => &value,
    };
    json_value_to_suggestions(target)
}

fn json_value_to_suggestions(v: &serde_json::Value) -> Vec<Suggestion> {
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(s) => Some(Suggestion {
                    name: s.clone(),
                    suggestion_type: SuggestionType::Arg,
                    priority: Some(60),
                    ..Default::default()
                }),
                serde_json::Value::Object(obj) => {
                    let name = obj.get("name")?.as_str()?.to_string();
                    Some(Suggestion {
                        name,
                        description: obj
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        suggestion_type: SuggestionType::Arg,
                        priority: Some(60),
                        ..Default::default()
                    })
                }
                _ => None,
            })
            .collect(),
        serde_json::Value::Object(obj) => {
            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                return vec![Suggestion {
                    name: name.to_string(),
                    description: obj
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    suggestion_type: SuggestionType::Arg,
                    priority: Some(60),
                    ..Default::default()
                }];
            }
            if let Some(packages) = obj.get("packages") {
                return json_value_to_suggestions(packages);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn cancelling_one_cache_waiter_keeps_the_shared_generator_alive() {
        let dir = crate::test_support::unique_temp_dir("shared-cache-cancel");
        let cwd = dir.to_str().unwrap();
        let command = Invocation::Shell(
            "echo start >> calls; while [ ! -f release ]; do sleep .02; done; printf ready".into(),
        );
        let key = CacheKey {
            command: command.clone(),
            cwd: Some(cwd.into()),
            separator: "\n".into(),
        };
        let policy = CacheSpec::default();
        let request = Cancellation::default();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                with_request(Some(request.clone()), || {
                    run_shell_line(&command, "\n", 5_000, Some(&policy), cwd)
                })
            });
            let second = scope.spawn(|| run_shell_line(&command, "\n", 5_000, Some(&policy), cwd));
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let entry = CACHE.lock().unwrap().get(&key).cloned();
                if entry.is_some_and(|entry| {
                    entry
                        .state
                        .lock()
                        .unwrap()
                        .in_flight
                        .get(&5_000)
                        .is_some_and(|flight| flight.waiters.load(Ordering::Relaxed) == 2)
                }) {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(POLL_INTERVAL);
            }
            request.cancel();
            assert!(first.join().unwrap().is_empty());
            std::fs::write(dir.join("release"), "").unwrap();
            assert_eq!(second.join().unwrap(), ["ready"]);
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("calls")).unwrap(),
            "start\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "subprocess fixture, invoked by windows_generator_releases_inherited_stdout"]
    fn windows_pipe_helper() {
        use std::io::Write;

        fn wait_for(path: &str) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !Path::new(path).exists() {
                assert!(Instant::now() < deadline, "waiting for {path}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        if std::env::var_os("INSH_TEST_PIPE_DESCENDANT").is_some() {
            std::fs::write("ready", "").unwrap();
            wait_for("release");
            let result = std::io::stdout().write_all(b"descendant output\n");
            std::fs::write("result", if result.is_err() { "closed" } else { "open" }).unwrap();
            // The libtest harness itself writes to stdout on return.
            std::process::exit(0);
        }
        let _descendant = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "generator::tests::windows_pipe_helper",
                "--nocapture",
            ])
            .env("INSH_TEST_PIPE_DESCENDANT", "1")
            .spawn()
            .unwrap();
        wait_for("ready");
        println!("generator output");
        if Path::new("time-out").exists() {
            wait_for("release");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_generator_releases_inherited_stdout() {
        let executable = std::env::current_exe()
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        let command = Invocation::Argv(vec![
            executable,
            "--ignored".into(),
            "--exact".into(),
            "generator::tests::windows_pipe_helper".into(),
            "--nocapture".into(),
        ]);
        for (time_out, cancel) in [(true, false), (true, false), (true, true), (false, false)] {
            let dir = crate::test_support::unique_temp_dir("generator-pipe");
            if time_out {
                std::fs::write(dir.join("time-out"), "").unwrap();
            }
            let output = if cancel {
                let request = Cancellation::default();
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while !dir.join("ready").exists() {
                            assert!(
                                Instant::now() < deadline,
                                "descendant was not ready for cancellation"
                            );
                            std::thread::sleep(POLL_INTERVAL);
                        }
                        request.cancel();
                    });
                    let start = Instant::now();
                    let result = with_request(Some(request.clone()), || {
                        run_command_with_timeout(&command, dir.to_str().unwrap(), 10_000)
                    });
                    assert!(
                        start.elapsed() < Duration::from_secs(4),
                        "cancellation waited for the timeout"
                    );
                    result
                })
            } else {
                run_command_with_timeout(&command, dir.to_str().unwrap(), 2_000)
            };
            let ready = dir.join("ready").exists();
            // Release the descendant even if reporting a failure below.
            std::fs::write(dir.join("release"), "").unwrap();
            let deadline = Instant::now() + Duration::from_secs(4);
            let state = loop {
                if let Ok(state) = std::fs::read_to_string(dir.join("result"))
                    && !state.is_empty()
                {
                    break state;
                }
                assert!(
                    Instant::now() < deadline,
                    "descendant did not report pipe status"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            std::fs::remove_dir_all(dir).unwrap();
            assert!(
                ready,
                "descendant did not inherit the generator pipe before cancellation"
            );
            assert_eq!(
                state, "closed",
                "descendant could still write to generator stdout"
            );
            if time_out {
                assert!(output.is_none());
            } else {
                let output = String::from_utf8(output.unwrap()).unwrap();
                assert!(output.contains("generator output"));
            }
        }
    }

    use super::*;

    /// #123: a leading separator is meaningful even when it is the only one.
    #[test]
    fn path_prefix_retains_directory_separator() {
        assert_eq!(split_path_prefix("/"), ("/", ""));
        assert_eq!(split_path_prefix("/tmp"), ("/", "tmp"));
        assert_eq!(split_path_prefix("nested/file"), ("nested/", "file"));
        #[cfg(windows)]
        {
            assert_eq!(split_path_prefix("C:\\"), ("C:\\", ""));
            assert_eq!(split_path_prefix("C:\\temp"), ("C:\\", "temp"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn absolute_root_paths_and_directory_symlinks_complete() {
        let dir = crate::test_support::unique_temp_dir("root-path");
        std::fs::write(dir.join("only-in-cwd"), "").unwrap();
        std::fs::create_dir(dir.join("folder")).unwrap();
        std::fs::write(dir.join("folder").join("child"), "").unwrap();
        std::os::unix::fs::symlink("folder", dir.join("link")).unwrap();
        std::os::unix::fs::symlink("missing", dir.join("broken")).unwrap();
        let cwd = dir.to_str().unwrap();

        // Root entries retain `/` and are independent of the current dir.
        let root = list_paths(cwd, "/", false, false);
        assert!(!root.is_empty());
        assert!(root.iter().all(|s| s.name.starts_with('/')));
        assert!(!root.iter().any(|s| s.name == "/only-in-cwd"));
        assert_eq!(
            list_paths(cwd, "/", false, false)
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            list_paths("/", "", false, false)
                .into_iter()
                .map(|s| format!("/{}", s.name))
                .collect::<Vec<_>>()
        );

        // The file-type fast path still follows links for folder completion.
        let folders = list_paths(cwd, "", true, false);
        assert_eq!(
            folders.into_iter().map(|s| s.name).collect::<Vec<_>>(),
            ["folder", "link"]
        );
        let nested = list_paths(cwd, "folder/", false, false);
        assert_eq!(
            nested.into_iter().map(|s| s.name).collect::<Vec<_>>(),
            ["folder/child"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    fn expire_cached_output(command: &Invocation, cwd: &str) -> Arc<CacheEntry> {
        let key = CacheKey {
            command: command.clone(),
            cwd: Some(cwd.into()),
            separator: "\n".into(),
        };
        let entry = CACHE.lock().unwrap().get(&key).unwrap().clone();
        entry.state.lock().unwrap().cached.as_mut().unwrap().at =
            Instant::now() - Duration::from_secs(60);
        entry
    }

    #[cfg(unix)]
    fn wait_for_cache_refresh(entry: &CacheEntry) {
        let start = Instant::now();
        while !entry.state.lock().unwrap().in_flight.is_empty() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "cache refresh did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_stale_refresh_is_asynchronous_and_shared() {
        let dir = scratch_dir("cache-138-stale");
        let cwd = dir.to_str().unwrap();
        let command = Invocation::Shell(
            "printf x >> runs; if test -e slow; then printf yes > started; while ! test -e release; do sleep 0.01; done; fi; cat value".into()
        );
        let cache = CacheSpec {
            strategy: CacheStrategy::StaleWhileRevalidate,
            ..Default::default()
        };
        std::fs::write(dir.join("value"), "old").unwrap();
        assert_eq!(
            run_shell_line(&command, "\n", 2000, Some(&cache), cwd),
            ["old"]
        );
        let entry = expire_cached_output(&command, cwd);
        std::fs::write(dir.join("value"), "new").unwrap();
        std::fs::write(dir.join("slow"), "").unwrap();
        let start = Instant::now();
        assert_eq!(
            run_shell_line(&command, "\n", 2000, Some(&cache), cwd),
            ["old"]
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "stale return waited for revalidation"
        );
        while !dir.join("started").exists() {
            assert!(start.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(5));
        }
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        run_shell_line(&command, "\n", 2000, Some(&cache), cwd)
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                assert_eq!(handle.join().unwrap(), ["old"]);
            }
        });
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xx");
        std::fs::write(dir.join("release"), "").unwrap();
        wait_for_cache_refresh(&entry);
        assert_eq!(
            run_shell_line(&command, "\n", 2000, Some(&cache), cwd),
            ["new"]
        );
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xx");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_refresh_failure_preserves_stale_output_and_allows_retry() {
        let dir = scratch_dir("cache-138-refresh-failure");
        let cwd = dir.to_str().unwrap();
        let command = Invocation::Shell(
            "printf x >> runs; if test -e fail; then exit 1; fi; cat value".into(),
        );
        let cache = CacheSpec {
            strategy: CacheStrategy::StaleWhileRevalidate,
            ..Default::default()
        };
        std::fs::write(dir.join("value"), "old").unwrap();
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&cache), cwd),
            ["old"]
        );
        let entry = expire_cached_output(&command, cwd);
        let cached_at = entry.state.lock().unwrap().cached.as_ref().unwrap().at;
        std::fs::write(dir.join("fail"), "").unwrap();
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&cache), cwd),
            ["old"]
        );
        wait_for_cache_refresh(&entry);
        {
            let state = entry.state.lock().unwrap();
            let cached = state.cached.as_ref().unwrap();
            assert_eq!(*cached.values, ["old"]);
            assert_eq!(cached.at, cached_at, "failure must not renew the stale TTL");
        }
        std::fs::remove_file(dir.join("fail")).unwrap();
        std::fs::write(dir.join("value"), "recovered").unwrap();
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&cache), cwd),
            ["old"]
        );
        wait_for_cache_refresh(&entry);
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&cache), cwd),
            ["recovered"]
        );
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "xxx");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_current_caller_ttl_and_zero_bypass() {
        let dir = scratch_dir("cache-138-ttl");
        let cwd = dir.to_str().unwrap();
        let command = Invocation::Shell("cat value".into());
        let long = CacheSpec::default();
        std::fs::write(dir.join("value"), "old").unwrap();
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&long), cwd),
            ["old"]
        );
        std::fs::write(dir.join("value"), "new").unwrap();
        let zero = CacheSpec {
            ttl_secs: 0,
            ..Default::default()
        };
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&zero), cwd),
            ["new"]
        );
        // Bypassing the cache must not replace another caller's cached value.
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&long), cwd),
            ["old"]
        );
        std::thread::sleep(Duration::from_millis(30));
        let short = CacheSpec {
            ttl_ms: Some(10),
            ..Default::default()
        };
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&short), cwd),
            ["new"]
        );
        std::fs::write(dir.join("value"), "newer").unwrap();
        let zero_ms = CacheSpec {
            ttl_ms: Some(0),
            ..Default::default()
        };
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&zero_ms), cwd),
            ["newer"]
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&long), cwd),
            ["new"]
        );
        assert_eq!(
            run_shell_line(&command, "\n", 1000, Some(&short), cwd),
            ["newer"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_failures_are_retried_but_successful_empty_output_is_cached() {
        let dir = scratch_dir("cache-138-failure");
        let cwd = dir.to_str().unwrap();
        let cache = CacheSpec::default();
        let fails = Invocation::Shell("if test -e value; then cat value; else exit 1; fi".into());
        assert!(run_shell_line(&fails, "\n", 1000, Some(&cache), cwd).is_empty());
        std::fs::write(dir.join("value"), "recovered").unwrap();
        assert_eq!(
            run_shell_line(&fails, "\n", 1000, Some(&cache), cwd),
            ["recovered"]
        );
        let empty = Invocation::Shell(
            "printf x >> runs; test -e empty-value && cat empty-value; exit 0".into(),
        );
        assert!(run_shell_line(&empty, "\n", 1000, Some(&cache), cwd).is_empty());
        std::fs::write(dir.join("empty-value"), "later").unwrap();
        assert!(run_shell_line(&empty, "\n", 1000, Some(&cache), cwd).is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "x");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_long_caller_does_not_inherit_a_short_producers_failure() {
        let dir = scratch_dir("cache-138-producer-timeout");
        let cwd = dir.to_str().unwrap();
        let command =
            Invocation::Shell("printf started > started; sleep 0.15; printf result".into());
        let cache = CacheSpec::default();
        std::thread::scope(|scope| {
            let producer = scope.spawn(|| run_shell_line(&command, "\n", 30, Some(&cache), cwd));
            let start = Instant::now();
            while !dir.join("started").exists() {
                assert!(start.elapsed() < Duration::from_secs(2));
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                run_shell_line(&command, "\n", 1000, Some(&cache), cwd),
                ["result"]
            );
            assert!(producer.join().unwrap().is_empty());
        });
        // The output remains shared across timeout policies after success.
        assert_eq!(
            run_shell_line(&command, "\n", 1, Some(&cache), cwd),
            ["result"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_waiting_caller_keeps_its_timeout() {
        let dir = scratch_dir("cache-138-wait-timeout");
        let cwd = dir.to_str().unwrap();
        let command = Invocation::Shell(
            "while ! test -e release; do printf started > started; sleep 0.01; done; printf result"
                .into(),
        );
        let cache = CacheSpec::default();
        let (started, short_output, owner_pending, released, waiter, owner) =
            std::thread::scope(|scope| {
                let owner =
                    scope.spawn(|| run_shell_line(&command, "\n", 30_000, Some(&cache), cwd));
                let start = Instant::now();
                while !dir.join("started").exists() && start.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let started = dir.join("started").exists();
                let (tx, rx) = std::sync::mpsc::channel();
                let waiter_command = &command;
                let waiter_cache = &cache;
                let waiter = scope.spawn(move || {
                    let output = run_shell_line(waiter_command, "\n", 20, Some(waiter_cache), cwd);
                    let _ = tx.send(output.clone());
                    output
                });
                let short_output = rx.recv_timeout(Duration::from_secs(5));
                let owner_pending = !owner.is_finished();
                // Release and join even when the short caller missed its outer
                // deadline, so an assertion cannot strand either subprocess.
                let released = std::fs::write(dir.join("release"), "");
                let waiter = waiter.join();
                let owner = owner.join();
                (
                    started,
                    short_output,
                    owner_pending,
                    released,
                    waiter,
                    owner,
                )
            });
        let cached = run_shell_line(&command, "\n", 20, Some(&cache), cwd);
        let removed = std::fs::remove_dir_all(dir);
        assert!(started, "producer did not reach the release gate");
        released.unwrap();
        removed.unwrap();
        assert!(
            owner_pending,
            "producer finished before the gate was released"
        );
        assert!(
            short_output
                .expect("short caller waited for the gated producer")
                .is_empty()
        );
        assert!(waiter.unwrap().is_empty());
        assert_eq!(owner.unwrap(), ["result"]);
        assert_eq!(cached, ["result"]);
    }

    #[cfg(unix)]
    #[test]
    fn cache_138_concurrent_cold_calls_share_one_execution() {
        let dir = scratch_dir("cache-138-concurrent");
        let command = Invocation::Shell("printf x >> runs; sleep 0.15; printf result".into());
        let cache = CacheSpec::default();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        run_shell_line(&command, "\n", 1000, Some(&cache), dir.to_str().unwrap())
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                assert_eq!(handle.join().unwrap(), ["result"]);
            }
        });
        assert_eq!(std::fs::read_to_string(dir.join("runs")).unwrap(), "x");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #113: same script, different separator must not share a cache entry.
    #[cfg(unix)]
    #[test]
    fn cache_key_includes_separator() {
        let cache = CacheSpec {
            ttl_secs: 60,
            by_dir: false,
            ..Default::default()
        };
        // Unique script text so no other test's cache entry can collide.
        let cmd = Invocation::Shell("printf 'a:b c:d' # cache-sep-test".into());
        let by_colon = run_shell_line(&cmd, ":", 5000, Some(&cache), ".");
        assert_eq!(by_colon, vec!["a", "b c", "d"]);
        let by_space = run_shell_line(&cmd, " ", 5000, Some(&cache), ".");
        assert_eq!(by_space, vec!["a:b", "c:d"]);
    }

    /// #117: the same joined argv text can represent different programs.
    #[cfg(unix)]
    #[test]
    fn cache_key_preserves_argv_boundaries_and_invocation_kind() {
        let cache = CacheSpec {
            ttl_secs: 60,
            ..Default::default()
        };
        let run = |command| run_shell_line(&command, "\n", 5000, Some(&cache), ".");
        let combined = Invocation::Argv(vec![
            "printf".into(),
            "%s\\n".into(),
            "cache-argv-117 a".into(),
        ]);
        let separate = Invocation::Argv(vec![
            "printf".into(),
            "%s\\n".into(),
            "cache-argv-117".into(),
            "a".into(),
        ]);
        assert_eq!(run(combined), ["cache-argv-117 a"]);
        assert_eq!(run(separate), ["cache-argv-117", "a"]);

        // Shell parsing discards a comment that direct argv passes literally.
        assert!(run(Invocation::Shell("printf %s #cache-kind-117".into())).is_empty());
        assert_eq!(
            run(Invocation::Argv(vec![
                "printf".into(),
                "%s".into(),
                "#cache-kind-117".into(),
            ])),
            ["#cache-kind-117"]
        );
    }

    /// #103: a FIFO must be skipped promptly, not block the open forever;
    /// an oversized regular file is likewise rejected.
    #[cfg(unix)]
    #[test]
    fn bounded_read_skips_fifo_and_oversized() {
        let dir = crate::test_support::unique_temp_dir("bounded");
        let fifo = dir.join("hang.json");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o644) };
        assert_eq!(rc, 0);
        let start = std::time::Instant::now();
        assert!(read_bounded_file(&fifo, 1024).is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let big = dir.join("big");
        std::fs::write(&big, vec![b'x'; 2048]).unwrap();
        assert!(read_bounded_file(&big, 1024).is_none());
        assert_eq!(read_bounded_file(&big, 4096).unwrap().len(), 2048);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The `history` template must read the history file of the shell being
    /// WRAPPED (`is start --shell fish` launched from bash), not whichever
    /// shell `detect()` finds in our own environment. fish stores `- cmd:`
    /// records, so loading with the wrong shell both reads the wrong file and
    /// leaks the raw record line into the suggestions.
    #[test]
    fn history_template_loads_the_wrapped_shells_history() {
        let dir = crate::test_support::unique_temp_dir("hist");
        let path = dir.join("history");
        std::fs::write(&path, "- cmd: git status\n  when: 1700000000\n").unwrap();

        let prev = std::env::var("HISTFILE").ok();
        // SAFETY: no other test in this binary reads HISTFILE; restored below.
        unsafe {
            std::env::set_var("HISTFILE", &path);
        }
        // Eight bundled specs route their main argument through this template
        // (ssh, scp, sftp, rsync, curl, mosh, awsume, preset).
        let arg = Arg {
            templates: vec![Template::History],
            ..Default::default()
        };
        let names = |shell| -> Vec<String> {
            suggestions_for_arg(&arg, ".", "", &[], true, &[], Some(shell))
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        let fish = names(Shell::Fish);
        let bash = names(Shell::Bash);
        match prev {
            Some(v) => unsafe { std::env::set_var("HISTFILE", v) },
            None => unsafe { std::env::remove_var("HISTFILE") },
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            fish.iter().any(|n| n == "git status"),
            "fish records were not parsed as commands: {fish:?}"
        );
        // Same bytes through the POSIX loader: the raw `- cmd:` record line
        // leaks through as the suggestion, which is what a wrapped-shell
        // mismatch used to surface.
        assert!(
            bash.iter().any(|n| n == "- cmd: git status"),
            "expected the fish record to leak through under the bash loader: {bash:?}"
        );
    }

    #[test]
    fn shell_line_timeout_returns_empty_quickly() {
        let start = Instant::now();
        let got = run_shell_line(
            &Invocation::Shell("sleep 2; echo late".into()),
            "\n",
            50,
            None,
            ".",
        );
        assert!(got.is_empty());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// #118: an exited shell must not leave a background process holding the
    /// output pipe, discarding complete output and consuming the full timeout.
    #[cfg(unix)]
    #[test]
    fn exited_generator_stops_background_descendants_and_preserves_output() {
        let dir = crate::test_support::unique_temp_dir("generator-descendant");
        let marker = dir.join("leaked");
        let script = "(sleep 0.3; printf leaked > leaked) & printf ready";
        let start = Instant::now();
        let got = run_shell_line(
            &Invocation::Shell(script.into()),
            "\n",
            2000,
            None,
            dir.to_str().unwrap(),
        );
        assert_eq!(got, ["ready"]);
        assert!(start.elapsed() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !marker.exists(),
            "background descendant survived completion"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Generator output and project-file values are data, not spec text, so
    /// they are marked for quoting on insertion; a spec's own suggestions are
    /// not (#86).
    #[cfg(unix)]
    #[test]
    fn generator_and_project_file_values_are_marked_external() {
        let dir = std::env::temp_dir().join(format!(
            "insh-external-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build;id":"tsc"}}"#,
        )
        .unwrap();
        let arg = Arg {
            suggestions: vec![Suggestion {
                name: "\\;".into(),
                ..Default::default()
            }],
            generators: vec![
                Generator::Script {
                    input: ScriptInput::Argv {
                        argv: vec!["printf".into(), "feat;id\\n".into()],
                    },
                    split_on: None,
                    post_process: PostProcess::default(),
                    timeout_ms: 5000,
                    cache: None,
                },
                Generator::ProjectFile {
                    reader: ProjectFileReader::PackageJsonScripts,
                },
            ],
            ..Default::default()
        };
        let got = suggestions_for_arg(&arg, dir.to_str().unwrap(), "", &[], false, &[], None);
        let _ = std::fs::remove_dir_all(&dir);
        let external = |name: &str| {
            got.iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("{name} missing from {got:?}"))
                .external
        };
        assert!(external("feat;id"));
        assert!(external("build;id"));
        assert!(!external("\\;"), "spec-authored text must stay verbatim");
    }

    #[test]
    fn json_object_packages_become_suggestions() {
        let got = json_to_suggestions(r#"{"packages":[{"name":"pkg-a"},{"name":"pkg-b"}]}"#, None);
        let names: Vec<String> = got.into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["pkg-a", "pkg-b"]);
    }

    /// A fresh, uniquely named directory for one test.
    #[cfg(unix)]
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "insh-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// Run `f` on a thread and fail, rather than hang the suite, if it does
    /// not return within a few seconds.
    #[cfg(unix)]
    fn within_seconds<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("completion blocked on a project file")
    }

    /// Project files belong to whoever authored the directory and are read on
    /// every keystroke. Only small regular files may be read: a FIFO used to
    /// block the suggestion worker forever, and a link to `/dev/zero` grew the
    /// buffer until the allocation aborted the wrapper (#87).
    #[cfg(unix)]
    #[test]
    fn project_files_are_read_only_when_small_and_regular() {
        // A FIFO named package.json / manage.py.
        let fifo_dir = scratch_dir("fifo");
        for name in ["package.json", "manage.py"] {
            let path = std::ffi::CString::new(fifo_dir.join(name).to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let cwd = fifo_dir.to_str().unwrap().to_string();
        let (scripts, clis, django) = within_seconds(move || {
            let django = Subcommand {
                names: vec!["django-admin".into()],
                ..Default::default()
            };
            (
                package_json_scripts(&cwd),
                package_json_node_clis(&cwd),
                file_exists_then("manage.py", Some("django"), &django, &cwd),
            )
        });
        assert!(scripts.is_empty() && clis.is_empty() && django.is_empty());

        // A link to an endless device.
        let zero_dir = scratch_dir("zero");
        std::os::unix::fs::symlink("/dev/zero", zero_dir.join("package.json")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", zero_dir.join("Cargo.toml")).unwrap();
        let cwd = zero_dir.to_str().unwrap().to_string();
        let (scripts, members) =
            within_seconds(move || (package_json_scripts(&cwd), cargo_workspace_members(&cwd)));
        assert!(scripts.is_empty() && members.is_empty());

        // An oversized manifest is ignored; a normal one still works.
        let big_dir = scratch_dir("big");
        let mut big = String::from(r#"{"scripts":{"build":"tsc"}}"#);
        big.push_str(&" ".repeat(MAX_MANIFEST_BYTES as usize));
        std::fs::write(big_dir.join("package.json"), big).unwrap();
        assert!(package_json_scripts(big_dir.to_str().unwrap()).is_empty());
        std::fs::write(
            big_dir.join("package.json"),
            r#"{"scripts":{"build":"tsc"}}"#,
        )
        .unwrap();
        let names: Vec<String> = package_json_scripts(big_dir.to_str().unwrap())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["build"]);

        for dir in [fifo_dir, zero_dir, big_dir] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The timeout bounds how long a generator runs, not how much it prints.
    /// Output over the cap is discarded and the generator is stopped at once
    /// rather than left to the timeout (#87).
    #[cfg(unix)]
    #[test]
    fn generator_output_is_capped() {
        let over = format!("yes | head -c {}", MAX_GENERATOR_OUTPUT + 1);
        assert!(run_command_with_timeout(&Invocation::Shell(over), ".", 10_000).is_none());

        let at_cap = format!("yes | head -c {MAX_GENERATOR_OUTPUT}");
        let bytes = run_command_with_timeout(&Invocation::Shell(at_cap), ".", 10_000).unwrap();
        assert_eq!(bytes.len(), MAX_GENERATOR_OUTPUT);

        // Endless output: stopped as soon as the cap is reached.
        let start = Instant::now();
        assert!(run_command_with_timeout(&Invocation::Shell("yes".into()), ".", 30_000).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "an endless generator ran for {:?}",
            start.elapsed()
        );
    }
}
