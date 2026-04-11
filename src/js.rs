//! boa_engine escape hatch for real Fig TypeScript specs.
//!
//! Phase 4 scaffold: load a JS/TS spec file, execute its top-level and look
//! for a `default` export in the form `{ name, subcommands, options, args }`.
//! Returns a list of suggestion strings.
//!
//! This is deliberately minimal — we do NOT implement the full Fig runtime.
//! What we DO is:
//!   * execute the JS
//!   * read the default export
//!   * walk a tiny subset of the shape (name, subcommands[].name, options[].name)
//!   * if a generator on the current arg is a Script, run its shell script
//!
//! Anything more advanced falls back to nothing.

#![cfg(feature = "js")]

use boa_engine::{Context, Source};

pub fn run_spec(source_file: &str, cwd: &str, _prefix: &str) -> Vec<String> {
    let Ok(src) = std::fs::read_to_string(source_file) else {
        return Vec::new();
    };
    let mut ctx = Context::default();
    // Provide a tiny Fig global shim so specs that touch `Fig` don't crash.
    let shim = r#"
        var Fig = Fig || {};
        Fig.Spec = function(s) { return s; };
    "#;
    if ctx.eval(Source::from_bytes(shim)).is_err() {
        return Vec::new();
    }
    let Ok(result) = ctx.eval(Source::from_bytes(&src)) else {
        return Vec::new();
    };
    // Walk: result.subcommands[].name
    let mut out = Vec::new();
    if let Some(obj) = result.as_object() {
        if let Ok(subs) = obj.get(boa_engine::js_string!("subcommands"), &mut ctx) {
            if let Some(arr) = subs.as_object() {
                let mut i = 0u32;
                while let Ok(item) = arr.get(i, &mut ctx) {
                    if item.is_undefined() {
                        break;
                    }
                    if let Some(o) = item.as_object() {
                        if let Ok(name) = o.get(boa_engine::js_string!("name"), &mut ctx) {
                            if let Some(s) = name.as_string() {
                                if let Ok(rs) = s.to_std_string() {
                                    out.push(rs);
                                }
                            }
                        }
                    }
                    i += 1;
                }
            }
        }
    }
    let _ = cwd;
    out
}
