//! Execution of Fig-spec generators.
//!
//! Handles `Generator::Script`, `Generator::Template`, and `Generator::Glob`.
//! `Custom(FnId)` and `PostProcess::Fn` fall through to empty until phase 6
//! wires the js_bridge. Results are cached per (cwd, script) with TTL.
//!
//! Phase 2 change: multiple generators per arg execute concurrently on
//! scoped threads. One-or-zero generator still takes the fast path.

use crate::spec::model::{
    Arg, CacheSpec, Generator, PostProcess, PostProcessKind, ScriptInput, Suggestion,
    SuggestionType, Template,
};
use once_cell::sync::Lazy;
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

static CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn suggestions_for_arg(arg: &Arg, cwd: &str, prefix: &str) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = arg.suggestions.clone();

    for tpl in &arg.templates {
        out.extend(template_suggestions(*tpl, cwd, prefix));
    }

    // Multi-generator fan-out: scoped threads run shell generators in
    // parallel. Single-generator arg stays on the caller's thread.
    match arg.generators.len() {
        0 => {}
        1 => {
            out.extend(run_generator(&arg.generators[0], cwd, prefix));
        }
        _ => {
            let results: Vec<Vec<Suggestion>> = std::thread::scope(|scope| {
                let handles: Vec<_> = arg
                    .generators
                    .iter()
                    .map(|g| scope.spawn(move || run_generator(g, cwd, prefix)))
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

fn run_generator(g: &Generator, cwd: &str, prefix: &str) -> Vec<Suggestion> {
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
        Generator::Template { template } => template_suggestions(*template, cwd, prefix),
        Generator::Glob { pattern } => glob_paths(pattern, cwd),
        Generator::Custom { .. } => Vec::new(),
    }
}

fn template_suggestions(tpl: Template, cwd: &str, prefix: &str) -> Vec<Suggestion> {
    match tpl {
        Template::Filepaths => list_paths(cwd, prefix, false),
        Template::Folders => list_paths(cwd, prefix, true),
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

fn list_paths(cwd: &str, prefix: &str, dirs_only: bool) -> Vec<Suggestion> {
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
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(file_prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if dirs_only && !is_dir {
            continue;
        }
        let joined = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        let (final_name, stype) = if is_dir {
            (format!("{joined}/"), SuggestionType::Folder)
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
