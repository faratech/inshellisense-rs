//! Execution of Fig-spec generators.
//!
//! Handles `Generator::Script`, `Generator::Template`, and `Generator::Glob`.
//! `Custom(FnId)` and `PostProcess::Fn` fall through to empty until phase 6
//! wires the js_bridge. Results are cached per (cwd, script) with TTL.
//!
//! Phase 2 change: multiple generators per arg execute concurrently on
//! scoped threads. One-or-zero generator still takes the fast path.

use crate::spec::model::{
    Arg, CacheSpec, Generator, PostProcess, PostProcessKind, ProjectFileReader, ScriptInput,
    Subcommand, Suggestion, SuggestionType, Template,
};
use std::sync::LazyLock;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct CacheEntry {
    at: Instant,
    ttl: Duration,
    values: Vec<Suggestion>,
}

static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn suggestions_for_arg(
    arg: &Arg,
    cwd: &str,
    prefix: &str,
    include_history: bool,
) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = arg.suggestions.clone();

    // Templates listed in the `templates` field do NOT add a `..` entry
    // (upstream `ls `/`vim ` omit it); the generator form does (see below).
    for tpl in &arg.templates {
        out.extend(template_suggestions(*tpl, cwd, prefix, include_history, false));
    }

    // Multi-generator fan-out: scoped threads run shell generators in
    // parallel. Single-generator arg stays on the caller's thread.
    match arg.generators.len() {
        0 => {}
        1 => {
            out.extend(run_generator(&arg.generators[0], cwd, prefix, include_history));
        }
        _ => {
            let results: Vec<Vec<Suggestion>> = std::thread::scope(|scope| {
                let handles: Vec<_> = arg
                    .generators
                    .iter()
                    .map(|g| scope.spawn(move || run_generator(g, cwd, prefix, include_history)))
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

fn run_generator(g: &Generator, cwd: &str, prefix: &str, include_history: bool) -> Vec<Suggestion> {
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
            template_suggestions(*template, cwd, prefix, include_history, true)
        }
        Generator::Glob { pattern } => glob_paths(pattern, cwd),
        Generator::Custom { .. } => Vec::new(), // always empty without JS
        Generator::ProjectFile { reader } => project_file_suggestions(*reader, cwd),
        Generator::FileExistsThen {
            path,
            content_contains,
            subcommand,
        } => file_exists_then(path, content_contains.as_deref(), subcommand, cwd),
    }
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
        let Ok(contents) = std::fs::read_to_string(&target) else {
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
    "vite", "vitest", "jest", "mocha", "ava", "tap", "eslint", "prettier",
    "tsc", "tslint", "webpack", "rollup", "parcel", "esbuild", "swc",
    "next", "nuxt", "remix", "astro", "gatsby", "vue-cli-service", "nx",
    "playwright", "cypress", "storybook", "babel", "babel-node", "ts-node",
    "tsx", "tap-spec", "nyc", "lerna", "rush", "pnpm", "yarn", "bun",
    "node", "nodemon", "concurrently", "husky", "lint-staged", "rimraf",
    "cross-env", "del-cli", "serve", "http-server", "browser-sync",
    "stylelint", "postcss", "sass", "less", "tailwindcss", "fastify",
    "nest", "hardhat", "truffle", "ganache", "fauna-shell", "wrangler",
    "vercel", "netlify", "supabase", "amplify", "firebase", "convex",
    "drizzle-kit", "prisma", "knex", "sequelize", "mongoose",
];

fn is_known_node_cli(name: &str) -> bool {
    NODE_CLIS.contains(&name)
}

fn project_file_suggestions(reader: ProjectFileReader, cwd: &str) -> Vec<Suggestion> {
    match reader {
        ProjectFileReader::PackageJsonScripts => package_json_scripts(cwd),
        ProjectFileReader::PackageJsonNodeClis => package_json_node_clis(cwd),
        ProjectFileReader::NodeModulesBinaries => node_modules_binaries(cwd),
        ProjectFileReader::CargoWorkspaceMembers => cargo_workspace_members(cwd),
    }
}

fn package_json_scripts(cwd: &str) -> Vec<Suggestion> {
    let pkg_path = std::path::Path::new(if cwd.is_empty() { "." } else { cwd })
        .join("package.json");
    let Ok(text) = std::fs::read_to_string(&pkg_path) else {
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
    let pkg_path = std::path::Path::new(if cwd.is_empty() { "." } else { cwd })
        .join("package.json");
    let Ok(text) = std::fs::read_to_string(&pkg_path) else {
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

fn node_modules_binaries(cwd: &str) -> Vec<Suggestion> {
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
                .filter(|n| is_known_node_cli(n))
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
    let path = std::path::Path::new(if cwd.is_empty() { "." } else { cwd })
        .join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
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
        Template::History => crate::history::load()
            .into_iter()
            .map(|h| Suggestion {
                name: h,
                suggestion_type: SuggestionType::Arg,
                priority: Some(55),
                ..Default::default()
            })
            .collect(),
        Template::Help => Vec::new(),
    }
}

fn list_paths(cwd: &str, prefix: &str, dirs_only: bool, add_parent: bool) -> Vec<Suggestion> {
    let base = Path::new(cwd);
    let (dir, file_prefix) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..idx], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let target = if dir.is_empty() {
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
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
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
            format!("{dir}/{name}")
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
    run_shell_line(&script_str, "\n", None, 5000, None, cwd)
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
    let (cmd_key, script_str) = match input {
        ScriptInput::Shell { script } => (script.clone(), script.clone()),
        ScriptInput::Argv { argv } => (argv.join(" "), shell_escape_argv(argv)),
        ScriptInput::FnTemplate { template } => {
            // Phase 4 will expand {tokens[N]} placeholders. For now treat as
            // a literal shell command.
            (template.clone(), template.clone())
        }
    };
    let sep = split_on.unwrap_or("\n");
    let raw = run_shell_line(&script_str, sep, Some(&cmd_key), timeout_ms, cache, cwd);
    apply_post_process(raw, post)
}

fn shell_escape_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(' ') || a.contains('"') || a.contains('\'') {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_shell_line(
    script: &str,
    sep: &str,
    cache_key: Option<&str>,
    _timeout_ms: u32,
    cache: Option<&CacheSpec>,
    cwd: &str,
) -> Vec<String> {
    let key = format!("{cwd}\0{}", cache_key.unwrap_or(script));
    let ttl = cache
        .map(|c| Duration::from_secs(c.ttl_secs))
        .unwrap_or(Duration::from_secs(30));

    {
        let c = CACHE.lock().unwrap();
        if let Some(entry) = c.get(&key) {
            if entry.at.elapsed() < entry.ttl {
                return entry.values.iter().map(|s| s.name.clone()).collect();
            }
        }
    }

    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(if cwd.is_empty() { "." } else { cwd })
        .output();
    let values: Vec<String> = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split(sep)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };

    let mut c = CACHE.lock().unwrap();
    c.insert(
        key,
        CacheEntry {
            at: Instant::now(),
            ttl,
            values: values
                .iter()
                .map(|v| Suggestion {
                    name: v.clone(),
                    ..Default::default()
                })
                .collect(),
        },
    );
    values
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
            .filter(|(_, name)| !name.is_empty())
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
                let desc = it.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
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
        _ => Vec::new(),
    }
}
